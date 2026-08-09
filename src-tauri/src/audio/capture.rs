use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Result of a successful recording
#[derive(Debug, Clone)]
pub struct RecordResult {
    pub file_path: std::path::PathBuf,
    pub duration_ms: u64,
    pub file_size_bytes: u64,
}

/// Audio capture events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum CaptureEvent {
    Started { sample_rate: u32 },
    Level { rms: f32 },
    Stopped { file_path: String, duration_ms: u64 },
    Error { message: String },
}

/// Shared state for stopping a recording
#[derive(Clone)]
pub struct RecordingHandle {
    pub stop: Arc<AtomicBool>,
}

impl RecordingHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Tracks the lifecycle of the PHYSICAL blocking capture worker so
/// recording-slot owners can prove it has terminated before releasing the
/// slot (P1-1). A `tokio::spawn` wrapper can be aborted without killing an
/// already-running `spawn_blocking` CPAL closure, so slot release must wait
/// on `wait()` rather than trusting the outer wrapper. `started`
/// distinguishes "a capture was in flight" from "the worker died before any
/// capture began" (e.g. aborted during TTS) — the latter must never block
/// the slot indefinitely.
#[derive(Clone, Default)]
pub struct CaptureCompletion {
    inner: Arc<tokio::sync::Notify>,
    started: Arc<AtomicBool>,
}

impl CaptureCompletion {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called when the blocking capture closure begins executing.
    pub fn mark_started(&self) {
        self.started.store(true, Ordering::SeqCst);
    }

    /// Whether a blocking capture was actually started.
    pub fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Wait until the blocking capture closure has exited. Signalled exactly
    /// once, on success or failure; the signal is stored if no waiter is
    /// present yet, so a late waiter still completes immediately.
    pub async fn wait(&self) {
        self.inner.notified().await;
    }

    /// Signal that the blocking closure has exited. Must be called at most
    /// once per completion.
    pub fn signal(&self) {
        self.inner.notify_one();
    }
}

/// Drop guard inside the blocking capture closure: signals `CaptureCompletion`
/// on EVERY exit path (success, `?`, bail, panic).
struct BlockingCaptureLifecycle(CaptureCompletion);

impl Drop for BlockingCaptureLifecycle {
    fn drop(&mut self) {
        self.0.signal();
    }
}

/// Bounded sample queue with a non-blocking producer for the real-time CPAL
/// callback. Overflow policy is explicit: queue available -> enqueue; queue
/// full -> drop the current chunk and count the overflow. The callback can
/// never block and never panics, so stop/cancellation can never be stalled
/// behind a full sample queue.
pub struct SampleQueue {
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    overflow: Arc<AtomicU64>,
}

impl SampleQueue {
    pub fn new(capacity: usize) -> (Self, std::sync::mpsc::Receiver<Vec<f32>>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(capacity);
        (
            Self {
                tx,
                overflow: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Non-blocking enqueue from a real-time callback. Never blocks; on a full
    /// queue the chunk is dropped and the overflow counter is incremented so
    /// the loss is observable/loggable.
    pub fn try_enqueue(&self, data: &[f32]) {
        if self.tx.try_send(data.to_vec()).is_err() {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Number of chunks dropped because the queue was full.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

/// P1-2 overflow policy: any dropped production audio chunk fails the
/// recording. Returns the user-facing error message when `dropped > 0` and
/// None when nothing was lost. A WAV missing part of the candidate's answer
/// must never be transcribed or persisted as valid evidence.
fn overflow_error(dropped: u64) -> Option<String> {
    if dropped > 0 {
        Some(format!(
            "Audio capture lost {} chunk(s); recording discarded. Please retry the round.",
            dropped
        ))
    } else {
        None
    }
}

/// Thread-safe latch for asynchronous capture errors. Written from the
/// real-time error callback; observed by the recording loop so a device
/// failure becomes an AUTHORITATIVE operation failure instead of a silent
/// success. `CaptureEvent::Error` remains supplementary UI information only.
#[derive(Clone, Default)]
pub struct CaptureErrorLatch {
    inner: Arc<std::sync::Mutex<Option<String>>>,
}

impl CaptureErrorLatch {
    /// Record the first error reported by the device callback.
    pub fn set(&self, message: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.is_none() {
                *guard = Some(message.into());
            }
        }
    }

    /// Non-consuming read of the latched error, if any.
    pub fn peek(&self) -> Option<String> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }
}

/// Pump chunks from `rx` into `on_chunk` until `target_frames` frames have
/// been collected, the wall-clock `deadline` passes (explicit timeout error),
/// an external `stop_flag` is set (explicit cancellation), a capture error is
/// latched (explicit device error), or the channel disconnects. Pure and
/// hardware-free, so it is unit-testable. Returns the number of frames
/// collected.
fn collect_chunks_until<F>(
    rx: &std::sync::mpsc::Receiver<Vec<f32>>,
    error_latch: &CaptureErrorLatch,
    stop_flag: Option<&AtomicBool>,
    deadline: std::time::Instant,
    target_frames: u64,
    channels: u16,
    mut on_chunk: F,
) -> anyhow::Result<u64>
where
    F: FnMut(&[f32]) -> anyhow::Result<()>,
{
    let mut total_frames = 0u64;
    while total_frames < target_frames {
        // Async device failure is authoritative — fail now, never succeed.
        if let Some(msg) = error_latch.peek() {
            anyhow::bail!("{}", msg);
        }
        // External cancellation (P2-2): the owning command can stop the
        // device-test capture promptly instead of waiting for the deadline.
        if let Some(flag) = stop_flag {
            if flag.load(Ordering::SeqCst) {
                anyhow::bail!("Capture cancelled");
            }
        }
        // Wall-clock bound independent of sample count: a stream that starts
        // but delivers no frames still terminates in bounded time.
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Audio capture timed out — no frames received before the wall-clock deadline"
            );
        }
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(samples) => {
                on_chunk(&samples)?;
                total_frames += samples.len() as u64 / channels as u64;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(total_frames)
}

/// Internal RAII cleanup for `record_to_wav` failure paths (P2-2). Removes
/// ONLY files this invocation created: the partial `.wav.tmp` (once the
/// writer has created it) and any provisional final WAV (once the temp has
/// been renamed to it) — when dropped without being committed. A pre-existing
/// final WAV is rejected before recording begins, so cleanup can never delete
/// evidence it did not create. Missing files are ignored and cleanup never
/// panics. Declared BEFORE the `WavWriter` so reverse declaration order
/// drops the writer (releasing its Windows file handle) before the guard
/// removes files.
struct PartialWavGuard {
    temp_path: PathBuf,
    final_path: PathBuf,
    owns_temp: bool,
    owns_final: bool,
    committed: bool,
}

impl PartialWavGuard {
    fn new(temp_path: PathBuf, final_path: PathBuf) -> Self {
        Self {
            temp_path,
            final_path,
            owns_temp: false,
            owns_final: false,
            committed: false,
        }
    }

    /// This invocation created the temp file — cleanup owns it.
    fn mark_temp_owned(&mut self) {
        self.owns_temp = true;
    }

    /// The temp was renamed to the final path — this invocation now owns the
    /// final (provisional until `commit`).
    fn mark_final_owned(&mut self) {
        self.owns_temp = false;
        self.owns_final = true;
    }

    /// Mark the recording fully committed — the final WAV must be preserved.
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PartialWavGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if self.owns_temp {
            let _ = std::fs::remove_file(&self.temp_path);
        }
        if self.owns_final {
            let _ = std::fs::remove_file(&self.final_path);
        }
    }
}

/// Start recording from the default microphone to a WAV file using cpal (WASAPI on Windows).
/// Writes to a temp file first, then renames on success for crash recovery.
/// Periodically flushes the writer so partial data survives a crash.
/// Returns `RecordResult` with file path, duration, and size on success.
pub async fn record_to_wav(
    output_path: PathBuf,
    sample_rate: u32,
    channels: u16,
    event_tx: mpsc::Sender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
    completion: CaptureCompletion,
) -> anyhow::Result<RecordResult> {
    let sr = sample_rate;
    let ch = channels;
    let temp_path = output_path.with_extension("wav.tmp");

    tokio::task::spawn_blocking(move || -> anyhow::Result<RecordResult> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        // P1-1: the physical capture lifecycle is signalled on every exit
        // path — slot owners wait on `completion` before releasing the slot.
        let _lifecycle = BlockingCaptureLifecycle(completion.clone());
        completion.mark_started();

        // Stale partial from a previous crashed run: remove it explicitly as
        // stale so this invocation starts clean (also when a final WAV
        // collision below is rejected). It is by definition uncommitted and
        // cannot be evidence.
        if temp_path.exists() {
            let _ = std::fs::remove_file(&temp_path);
        }

        // P1-2: a pre-existing final WAV is evidence — never overwrite or
        // delete it. Reject BEFORE any destructive cleanup ownership begins
        // (and before any audio device is touched).
        if output_path.exists() {
            anyhow::bail!("Recording output already exists: {}", output_path.display());
        }

        // Own partial-file cleanup for EVERY failure path: any `?`/bail below
        // unwinds with the guard armed, and because the guard is declared
        // before the writer, the writer (holding the file handle) drops first.
        // The guard only ever removes files THIS invocation created.
        let mut cleanup = PartialWavGuard::new(temp_path.clone(), output_path.clone());

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device found"))?;

        let config = cpal::StreamConfig {
            channels: ch,
            sample_rate: cpal::SampleRate(sr),
            buffer_size: cpal::BufferSize::Default,
        };

        let spec = hound::WavSpec {
            channels: ch,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(&temp_path, spec)?;
        // The temp file now exists because of THIS invocation.
        cleanup.mark_temp_owned();

        let (sample_queue, sample_rx) = SampleQueue::new(64);
        let overflow_watch = sample_queue.overflow.clone();
        let error_latch = CaptureErrorLatch::default();
        let err_latch_cb = error_latch.clone();
        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                // Non-blocking: never stall the real-time callback on a full
                // queue. Full -> drop chunk + count overflow.
                sample_queue.try_enqueue(data);
            },
            move |err| {
                eprintln!("Input stream error: {}", err);
                let msg = format!("Audio stream error: {}", err);
                err_latch_cb.set(msg.clone());
                let _ = err_tx.try_send(CaptureEvent::Error { message: msg });
            },
            None,
        )?;

        stream.play()?;
        let _ = event_tx.try_send(CaptureEvent::Started { sample_rate: sr });

        let mut total_frames = 0u64;
        let mut chunks_since_flush = 0u32;
        const FLUSH_INTERVAL: u32 = 50;

        while !stop_flag.load(Ordering::SeqCst) {
            // Async capture failure is authoritative — stop the stream, drop
            // the writer (release the file handle), and fail the recording.
            // The armed guard removes the partial temp file.
            if let Some(msg) = error_latch.peek() {
                drop(stream);
                drop(writer);
                anyhow::bail!("{}", msg);
            }
            match sample_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(samples) => {
                    let rms = if samples.is_empty() {
                        0.0
                    } else {
                        let sum: f32 = samples.iter().map(|s| s * s).sum();
                        (sum / samples.len() as f32).sqrt()
                    };
                    let _ = event_tx.try_send(CaptureEvent::Level { rms });

                    for &sample in &samples {
                        let i16_sample = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        writer.write_sample(i16_sample)?;
                    }
                    total_frames += samples.len() as u64 / ch as u64;

                    chunks_since_flush += 1;
                    if chunks_since_flush >= FLUSH_INTERVAL {
                        writer.flush()?;
                        chunks_since_flush = 0;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        drop(stream);
        let dropped = overflow_watch.load(Ordering::Relaxed);
        if let Some(msg) = overflow_error(dropped) {
            // P1-2: any dropped production audio chunk fails the round. Drop
            // the writer first so the file handles are released; the armed
            // guard then removes the partial temp file AND any provisional
            // final WAV. Surface a CaptureEvent::Error and return Err — never
            // a successful RecordResult, so no Whisper and no DB persistence
            // can follow.
            drop(writer);
            let _ = event_tx.try_send(CaptureEvent::Error {
                message: msg.clone(),
            });
            anyhow::bail!("{}", msg);
        }
        writer.finalize()?;

        // Atomic rename: temp -> final (crash-safe). From here the invocation
        // owns the final path; a post-rename failure removes only this
        // provisional final.
        std::fs::rename(&temp_path, &output_path)?;
        cleanup.mark_final_owned();

        let duration_ms = (total_frames * 1000) / sr as u64;
        let file_size_bytes = std::fs::metadata(&output_path)?.len();
        // Fully committed — the final WAV must be preserved.
        cleanup.commit();
        let _ = event_tx.try_send(CaptureEvent::Stopped {
            file_path: output_path.to_string_lossy().to_string(),
            duration_ms,
        });

        Ok(RecordResult {
            file_path: output_path,
            duration_ms,
            file_size_bytes,
        })
    })
    .await?
}

/// List available audio input devices
pub async fn list_input_devices() -> anyhow::Result<Vec<String>> {
    tokio::task::spawn_blocking(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let devices = host
            .input_devices()?
            .filter_map(|d| d.name().ok().map(|n| n.to_string()))
            .collect();
        Ok(devices)
    })
    .await?
}

/// List available audio output devices
pub async fn list_output_devices() -> anyhow::Result<Vec<String>> {
    tokio::task::spawn_blocking(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let devices = host
            .output_devices()?
            .filter_map(|d| d.name().ok().map(|n| n.to_string()))
            .collect();
        Ok(devices)
    })
    .await?
}

/// Name of the DEFAULT output device — the one production playback actually
/// uses (Piper TTS and round playback both select cpal's
/// `default_output_device()`). Returns `Ok(None)` when the host has no
/// default output; this is deliberately distinct from listing all output
/// devices, because a non-default speaker alone must not mark TTS
/// readiness (P2-1).
pub async fn get_default_output_device_name() -> anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        Ok(host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .map(|n| n.to_string()))
    })
    .await?
}

/// Record a short audio clip for device verification (3 seconds max).
/// Returns the path to the recorded temp file.
/// The capture is bounded by BOTH the expected sample count AND a hard
/// wall-clock deadline (duration + 2s): if the stream starts but delivers no
/// frames, the check terminates with an explicit timeout error instead of
/// looping forever.
pub async fn record_test_clip(
    sample_rate: u32,
    channels: u16,
    duration_secs: u32,
    temp_dir: PathBuf,
    event_tx: mpsc::Sender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
    completion: CaptureCompletion,
) -> anyhow::Result<PathBuf> {
    // P2-1: UUID-based name so concurrent device checks can never target the
    // same path (a PID alone is not unique across concurrent checks).
    let tmp_path = temp_dir.join(format!("device_test_{}.wav", uuid::Uuid::new_v4()));
    let path_clone = tmp_path.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        // P1-1: signal the physical capture lifecycle on every exit path.
        let _lifecycle = BlockingCaptureLifecycle(completion.clone());
        completion.mark_started();

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device found"))?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(&path_clone, spec)?;
        let (sample_queue, sample_rx) = SampleQueue::new(64);
        let overflow_watch = sample_queue.overflow.clone();
        let error_latch = CaptureErrorLatch::default();
        let err_latch_cb = error_latch.clone();
        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                // Non-blocking: never stall the real-time callback on a full
                // queue. Full -> drop chunk + count overflow.
                sample_queue.try_enqueue(data);
            },
            move |err| {
                let msg = format!("Test clip error: {}", err);
                err_latch_cb.set(msg.clone());
                let _ = err_tx.try_send(CaptureEvent::Error { message: msg });
            },
            None,
        )?;

        stream.play()?;
        let _ = event_tx.try_send(CaptureEvent::Started { sample_rate });

        let total_needed = sample_rate as u64 * duration_secs as u64;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(duration_secs as u64 + 2);

        let collected = collect_chunks_until(
            &sample_rx,
            &error_latch,
            Some(&stop_flag),
            deadline,
            total_needed,
            channels,
            |samples| {
                let rms = if samples.is_empty() {
                    0.0
                } else {
                    let sum: f32 = samples.iter().map(|s| s * s).sum();
                    (sum / samples.len() as f32).sqrt()
                };
                let _ = event_tx.try_send(CaptureEvent::Level { rms });

                for &sample in samples {
                    let i16_sample = (sample * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    writer.write_sample(i16_sample)?;
                }
                Ok(())
            },
        );

        // Stream is done regardless of outcome — drop it before handling the
        // result so device error/timeout paths release the device promptly.
        drop(stream);
        let total_frames = match collected {
            Ok(frames) => frames,
            Err(e) => {
                // Windows file-handle ordering (P2-2): drop the writer FIRST
                // so its file handle is released, THEN remove the partial
                // file. Removing a file while the writer still holds the
                // handle commonly fails on Windows and leaves a stray
                // device_test_*.wav behind.
                drop(writer);
                let _ = std::fs::remove_file(&path_clone);
                return Err(e);
            }
        };

        // Overflow is observable/loggable — the loss is counted, never hidden.
        let dropped = overflow_watch.load(Ordering::Relaxed);
        if dropped > 0 {
            eprintln!(
                "[capture] device test finished with {} dropped chunk(s) (sample queue full)",
                dropped
            );
        }
        writer.finalize()?;

        let duration_ms = (total_frames * 1000) / sample_rate as u64;
        let _ = event_tx.try_send(CaptureEvent::Stopped {
            file_path: path_clone.to_string_lossy().to_string(),
            duration_ms,
        });

        Ok(())
    })
    .await??;

    // Verify the file was created and has content. A too-short clip is a
    // failed test, not evidence: delete the file so nothing is left behind.
    let metadata = std::fs::metadata(&tmp_path)?;
    if metadata.len() < 100 {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::bail!("Test clip too short ({} bytes)", metadata.len());
    }

    Ok(tmp_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_handle_stop_sets_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let handle = RecordingHandle { stop: flag.clone() };
        assert!(!flag.load(Ordering::SeqCst));
        handle.stop();
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn recording_handle_clone_shares_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let handle1 = RecordingHandle { stop: flag.clone() };
        let handle2 = handle1.clone();
        handle2.stop();
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn capture_event_started_serializes() {
        let event = CaptureEvent::Started { sample_rate: 44100 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("Started"));
        assert!(json.contains("44100"));
    }

    #[test]
    fn capture_event_level_serializes() {
        let event = CaptureEvent::Level { rms: 0.5 };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("Level"));
        assert!(json.contains("0.5"));
    }

    #[test]
    fn capture_event_error_serializes() {
        let event = CaptureEvent::Error {
            message: "test error".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("Error"));
        assert!(json.contains("test error"));
    }

    /// P1-2: the overflow policy fails production capture on ANY dropped
    /// chunk — incomplete evidence can never be marked successful.
    #[test]
    fn overflow_policy_fails_recording_on_any_dropped_chunk() {
        assert!(overflow_error(0).is_none(), "no loss must not fail");
        let msg = overflow_error(2).unwrap();
        assert!(
            msg.contains("lost 2 chunk(s)"),
            "must report the dropped count, got: {}",
            msg
        );
        assert!(
            msg.contains("recording discarded"),
            "must clearly discard the recording, got: {}",
            msg
        );
        assert!(
            msg.contains("Please retry the round"),
            "must tell the user to retry, got: {}",
            msg
        );
    }

    /// P2-2: an armed cleanup guard removes the partial temp file it created.
    #[test]
    fn partial_wav_guard_armed_removes_temp() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("round.wav.tmp");
        let final_path = dir.path().join("round.wav");
        std::fs::write(&temp, b"partial").unwrap();

        {
            let mut guard = PartialWavGuard::new(temp.clone(), final_path.clone());
            guard.mark_temp_owned();
        }
        assert!(!temp.exists(), "armed guard must remove the temp file");
    }

    /// P2-2: an uncommitted provisional final WAV owned by this invocation is
    /// also removed (the final is provisional until the caller commits it —
    /// e.g. after DB commit). After the rename the temp no longer exists, so
    /// only the provisional final is owned.
    #[test]
    fn partial_wav_guard_armed_removes_uncommitted_final() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("round.wav.tmp");
        let final_path = dir.path().join("round.wav");
        std::fs::write(&final_path, b"provisional").unwrap();

        {
            let mut guard = PartialWavGuard::new(temp.clone(), final_path.clone());
            guard.mark_temp_owned();
            guard.mark_final_owned();
        }
        assert!(!temp.exists(), "temp is gone after the rename lifecycle");
        assert!(
            !final_path.exists(),
            "uncommitted provisional final must be removed"
        );
    }

    /// P1-2 core invariant: a final WAV this invocation did NOT create is
    /// never removed — the guard only owns what it marked.
    #[test]
    fn partial_wav_guard_never_removes_unowned_final() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("round.wav.tmp");
        let final_path = dir.path().join("round.wav");
        let original = b"pre-existing-evidence";
        std::fs::write(&temp, b"partial").unwrap();
        std::fs::write(&final_path, original).unwrap();

        {
            let mut guard = PartialWavGuard::new(temp.clone(), final_path.clone());
            // Only the temp was created by this invocation.
            guard.mark_temp_owned();
        }
        assert!(!temp.exists(), "owned temp removed");
        assert!(final_path.exists(), "unowned final must survive");
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            original,
            "unowned final must be byte-for-byte unchanged"
        );
    }

    /// P2-2: a committed guard preserves the final WAV.
    #[test]
    fn partial_wav_guard_committed_preserves_final() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("round.wav.tmp");
        let final_path = dir.path().join("round.wav");
        std::fs::write(&final_path, b"committed-evidence").unwrap();

        {
            let mut guard = PartialWavGuard::new(temp.clone(), final_path.clone());
            guard.commit();
        }
        assert!(final_path.exists(), "committed final WAV must be preserved");
        assert_eq!(std::fs::read(&final_path).unwrap(), b"committed-evidence");
    }

    /// P2-2: cleanup of missing files is harmless and never panics.
    #[test]
    fn partial_wav_guard_missing_files_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let temp = dir.path().join("never-created.wav.tmp");
        let final_path = dir.path().join("never-created.wav");

        // Must not panic and must not error.
        let _guard = PartialWavGuard::new(temp.clone(), final_path.clone());
        drop(_guard);
    }

    /// P1-2: a pre-existing final WAV is rejected BEFORE any audio device is
    /// touched, the original file stays byte-for-byte unchanged, and a stale
    /// temp from a previous run is removed by policy.
    #[tokio::test]
    async fn record_to_wav_rejects_existing_final_before_audio() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("round.wav");
        let original = b"pre-existing-evidence";
        std::fs::write(&final_path, original).unwrap();
        let temp_path = final_path.with_extension("wav.tmp");
        std::fs::write(&temp_path, b"stale").unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let result = record_to_wav(
            final_path.clone(),
            16000,
            1,
            tx,
            Arc::new(AtomicBool::new(false)),
            CaptureCompletion::new(),
        )
        .await;

        assert!(result.is_err(), "existing final must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("already exists"), "unexpected error: {}", msg);
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            original,
            "original WAV must be byte-for-byte unchanged"
        );
        assert!(
            !temp_path.exists(),
            "stale temp must be removed by the stale-temp policy"
        );
    }

    /// A full sample queue never blocks the producer: the chunk is dropped and
    /// the overflow is counted, while the queued chunk is still delivered.
    #[test]
    fn sample_queue_full_drops_and_counts_overflow() {
        let (queue, rx) = SampleQueue::new(1);
        queue.try_enqueue(&[1.0, 2.0]);
        // Queue is full — this must NOT block; the chunk is dropped + counted.
        queue.try_enqueue(&[3.0, 4.0]);
        assert_eq!(queue.overflow_count(), 1);
        // The first chunk is still delivered intact.
        assert_eq!(rx.recv().unwrap(), vec![1.0, 2.0]);
        // Room is available again — enqueue succeeds without more overflow.
        queue.try_enqueue(&[5.0]);
        assert_eq!(queue.overflow_count(), 1);
        assert_eq!(rx.recv().unwrap(), vec![5.0]);
    }

    /// The producer path never blocks even with a full queue and no consumer.
    #[test]
    fn sample_queue_never_blocks_producer_when_full() {
        let (queue, _rx) = SampleQueue::new(2);
        queue.try_enqueue(&[1.0]);
        queue.try_enqueue(&[2.0]);
        // Full — the following calls return immediately (structural guarantee
        // of try_send) and only count overflow.
        queue.try_enqueue(&[3.0]);
        queue.try_enqueue(&[4.0]);
        assert_eq!(queue.overflow_count(), 2);
    }

    #[test]
    fn capture_error_latch_sets_once_and_peeks() {
        let latch = CaptureErrorLatch::default();
        assert!(latch.peek().is_none());
        latch.set("first");
        latch.set("second");
        // First error wins; peek is non-consuming.
        assert_eq!(latch.peek().as_deref(), Some("first"));
        assert_eq!(latch.peek().as_deref(), Some("first"));
    }

    /// A latched capture error becomes an authoritative failure: the loop
    /// returns Err instead of succeeding, in bounded time.
    #[test]
    fn collect_chunks_until_surfaces_latched_capture_error() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
        let latch = CaptureErrorLatch::default();
        latch.set("Audio stream error: simulated device failure");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let result = collect_chunks_until(&rx, &latch, None, deadline, 100, 1, |_| Ok(()));
        assert!(result.is_err(), "latched error must fail the capture loop");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("simulated device failure"),
            "expected the latched message, got: {}",
            msg
        );
        drop(tx);
    }

    /// Zero-frame case: a stream that delivers no chunks must terminate in
    /// bounded time with an explicit timeout error (P1-4 acceptance).
    #[test]
    fn collect_chunks_until_times_out_when_no_frames_arrive() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        let started = std::time::Instant::now();

        let result = collect_chunks_until(
            &rx,
            &CaptureErrorLatch::default(),
            None,
            deadline,
            16000,
            1,
            |_| Ok(()),
        );
        assert!(result.is_err(), "no frames + deadline must error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timed out"),
            "timeout error must be explicit, got: {}",
            msg
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "zero-frame case must exit in bounded time"
        );
        drop(tx);
    }

    /// Normal capture: chunks arrive, target frame count is reached, frames
    /// are written.
    #[test]
    fn collect_chunks_until_succeeds_when_frames_arrive() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(8);
        let mut written: Vec<f32> = Vec::new();

        let sender = std::thread::spawn(move || {
            tx.send(vec![1.0; 160]).unwrap();
            tx.send(vec![2.0; 160]).unwrap();
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let frames = collect_chunks_until(
            &rx,
            &CaptureErrorLatch::default(),
            None,
            deadline,
            320,
            1,
            |samples| {
                written.extend_from_slice(samples);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(frames, 320);
        assert_eq!(written.len(), 320);

        sender.join().unwrap();
    }

    /// P2-2: an outer cancellation (stop flag) terminates the capture loop
    /// promptly with an explicit error — the device test observes the owning
    /// command's cancellation instead of running until its deadline.
    #[test]
    fn collect_chunks_until_stops_on_outer_cancellation() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(4);
        let stop = Arc::new(AtomicBool::new(false));
        stop.store(true, Ordering::SeqCst);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let started = std::time::Instant::now();
        let result = collect_chunks_until(
            &rx,
            &CaptureErrorLatch::default(),
            Some(&stop),
            deadline,
            16000,
            1,
            |_| Ok(()),
        );
        assert!(result.is_err(), "outer cancellation must fail the loop");
        assert_eq!(
            result.unwrap_err().to_string(),
            "Capture cancelled",
            "cancellation must be explicit"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "cancellation must be prompt, not deadline-bound"
        );
        drop(tx);
    }
}
