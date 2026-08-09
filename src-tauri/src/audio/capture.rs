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
/// a capture error is latched (explicit device error), or the channel
/// disconnects. Pure and hardware-free, so it is unit-testable. Returns the
/// number of frames collected.
fn collect_chunks_until<F>(
    rx: &std::sync::mpsc::Receiver<Vec<f32>>,
    error_latch: &CaptureErrorLatch,
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
) -> anyhow::Result<RecordResult> {
    let sr = sample_rate;
    let ch = channels;
    let temp_path = output_path.with_extension("wav.tmp");

    tokio::task::spawn_blocking(move || -> anyhow::Result<RecordResult> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

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
            // the partial temp file, and fail the recording.
            if let Some(msg) = error_latch.peek() {
                drop(stream);
                let _ = std::fs::remove_file(&temp_path);
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
            // the writer first so the file handles are released, then remove
            // the partial temp file and any provisional final WAV, surface a
            // CaptureEvent::Error, and return Err — never a successful
            // RecordResult, so no Whisper and no DB persistence can follow.
            drop(writer);
            let _ = std::fs::remove_file(&temp_path);
            let _ = std::fs::remove_file(&output_path);
            let _ = event_tx.try_send(CaptureEvent::Error {
                message: msg.clone(),
            });
            anyhow::bail!("{}", msg);
        }
        writer.finalize()?;

        // Atomic rename: temp -> final (crash-safe)
        std::fs::rename(&temp_path, &output_path)?;

        let duration_ms = (total_frames * 1000) / sr as u64;
        let file_size_bytes = std::fs::metadata(&output_path)?.len();
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
) -> anyhow::Result<PathBuf> {
    // P2-1: UUID-based name so concurrent device checks can never target the
    // same path (a PID alone is not unique across concurrent checks).
    let tmp_path = temp_dir.join(format!("device_test_{}.wav", uuid::Uuid::new_v4()));
    let path_clone = tmp_path.clone();

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

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
        let result = collect_chunks_until(&rx, &latch, deadline, 100, 1, |_| Ok(()));
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
}
