use crate::audio::pipe::StderrDrain;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Piper outputs raw PCM at 22050 Hz mono — tied to the en_US-amy-medium model
const PIPER_SAMPLE_RATE: u32 = 22050;

/// Maximum time allowed for a single playback operation before it is cancelled.
/// Shared with the Piper supervisor's raw-PCM playback, which uses the same
/// absolute bound for a stalled output device.
pub(crate) const PLAYBACK_TIMEOUT_SECS: u64 = 120;

/// Process-level timeout for standalone TTS generation. The child process owner
/// enforces this directly — any outer timeout is only defense-in-depth.
const GENERATE_TTS_TIMEOUT_SECS: u64 = 25;

/// Maximum chunk size for reading Piper stdout in bounded reads.
const STDOUT_CHUNK_SIZE: usize = 8192;

/// Playback events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum PlaybackEvent {
    Started { duration_ms: u64 },
    Completed,
    Cancelled,
    Error { message: String },
}

/// Wait for playback to finish, be cancelled, or fail. Completion is based on
/// ACTUAL sample progress (P2-3): success only once the output callback has
/// consumed `total_samples` samples. Elapsed expected duration alone is NOT
/// proof of successful playback — a stalled output device (no progress) or a
/// latched output-stream error fails the operation.
/// Returns Ok on completion or controlled cancellation (stop flag); Err on a
/// latched output error or when the absolute `deadline` passes without
/// progress.
pub fn await_playback(
    playback_err: &std::sync::Mutex<Option<String>>,
    stop_flag: Option<&AtomicBool>,
    deadline: std::time::Instant,
    position: &std::sync::atomic::AtomicUsize,
    total_samples: usize,
) -> anyhow::Result<()> {
    loop {
        if let Some(msg) = playback_err.lock().map(|g| g.clone()).unwrap_or_default() {
            anyhow::bail!("{}", msg);
        }
        if position.load(Ordering::Relaxed) >= total_samples {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("Playback timed out — output device did not make progress");
        }
        if let Some(flag) = stop_flag {
            if flag.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Play a WAV file through the system speakers using cpal.
/// Respects `stop_flag` for cancellation and enforces an internal timeout.
/// On timeout or stop, the audio stream is dropped (stopping playback) and
/// `Cancelled` is emitted.
pub async fn play_wav(
    file_path: PathBuf,
    event_tx: mpsc::Sender<PlaybackEvent>,
    stop_flag: Option<Arc<AtomicBool>>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();

        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("No output device found"))?;

        // Read the WAV file with hound
        let mut reader = hound::WavReader::open(&file_path)?;
        let spec = reader.spec();

        let config = cpal::StreamConfig {
            channels: spec.channels,
            sample_rate: cpal::SampleRate(spec.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let duration_ms = {
            let data_size = std::fs::metadata(&file_path)?.len() as u64;
            let bytes_per_sample = spec.bits_per_sample as u64 / 8;
            let total_samples = data_size / bytes_per_sample;
            let total_frames = total_samples / spec.channels as u64;
            (total_frames * 1000) / spec.sample_rate as u64
        };

        let _ = event_tx.try_send(PlaybackEvent::Started { duration_ms });

        // Collect all samples into a Vec<f32>
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => reader
                .samples::<i16>()
                .filter_map(|s| s.ok())
                .map(|s| s as f32 / 32768.0)
                .collect(),
            hound::SampleFormat::Float => reader.samples::<f32>().filter_map(|s| s.ok()).collect(),
        };

        let samples = std::sync::Arc::new(samples);
        let pos = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let samples_clone = samples.clone();
        let pos_clone = pos.clone();

        // Latch for asynchronous output-stream errors. The error callback
        // writes here; the playback loop observes it so a device failure after
        // stream.play() succeeds still fails the operation (P1-5).
        let playback_err: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let err_latch = playback_err.clone();
        let err_tx = event_tx.clone();
        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let start = pos_clone.load(std::sync::atomic::Ordering::Relaxed);
                for (i, sample) in data.iter_mut().enumerate() {
                    let idx = start + i;
                    *sample = if idx < samples_clone.len() {
                        samples_clone[idx]
                    } else {
                        0.0 // silence after end
                    };
                }
                pos_clone.fetch_add(data.len(), std::sync::atomic::Ordering::Relaxed);
            },
            move |err| {
                eprintln!("Output stream error: {}", err);
                let msg = format!("Playback error: {}", err);
                if let Ok(mut guard) = err_latch.lock() {
                    if guard.is_none() {
                        *guard = Some(msg.clone());
                    }
                }
                let _ = err_tx.try_send(PlaybackEvent::Error { message: msg });
            },
            None,
        )?;

        stream.play()?;

        // Wait for playback to finish, with cancellation and timeout.
        let total_samples = samples.len();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(PLAYBACK_TIMEOUT_SECS);
        loop {
            // Async output error is authoritative — never report success after
            // the device failed.
            if let Some(msg) = playback_err.lock().map(|g| g.clone()).unwrap_or_default() {
                drop(stream);
                anyhow::bail!("{}", msg);
            }
            let current = pos.load(std::sync::atomic::Ordering::Relaxed);
            if current >= total_samples {
                break;
            }
            // Check external stop flag
            if let Some(ref flag) = stop_flag {
                if flag.load(Ordering::SeqCst) {
                    drop(stream);
                    let _ = event_tx.try_send(PlaybackEvent::Cancelled);
                    return Ok(());
                }
            }
            // Check internal deadline
            if std::time::Instant::now() >= deadline {
                drop(stream);
                let _ = event_tx.try_send(PlaybackEvent::Error {
                    message: format!("Playback timed out after {}s", PLAYBACK_TIMEOUT_SECS),
                });
                anyhow::bail!("Playback timed out after {}s", PLAYBACK_TIMEOUT_SECS);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        drop(stream);
        let _ = event_tx.try_send(PlaybackEvent::Completed);

        Ok(())
    })
    .await?
}

/// Generate a TTS WAV file using Piper via stdin.
/// The child process owner enforces timeout, kill, wait/reap, and partial
/// output cleanup directly. Any outer timeout is only defense-in-depth.
pub async fn generate_tts(
    text: &str,
    output_path: PathBuf,
    piper_binary: Option<&str>,
    model_path: Option<&str>,
) -> anyhow::Result<()> {
    let piper = piper_binary.unwrap_or("piper");
    let model = model_path.unwrap_or("en_US-amy-medium.onnx");

    let mut child = Command::new(piper)
        .arg("--model")
        .arg(model)
        .arg("--output-raw")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Drain stderr concurrently so Piper can never block on a full stderr
    // pipe while we consume its stdout PCM.
    let stderr_drain = child.stderr.take().map(StderrDrain::start);

    // ONE absolute deadline for the whole process lifecycle (P2-4): stdout
    // reads, cancellation selection, child exit, and the final wait all share
    // this budget — EOF does not reset the timeout.
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(GENERATE_TTS_TIMEOUT_SECS);

    // Send text via stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(text.as_bytes()).await {
            // Child may still be running — kill and reap explicitly.
            crate::audio::pipe::terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&output_path);
            anyhow::bail!("Failed to write TTS text to Piper stdin: {}", e);
        }
        drop(stdin); // close stdin to signal EOF
    }

    // Read stdout with async bounded reads and deadline enforcement
    let mut raw_pcm = Vec::new();
    let mut stdout = child.stdout.take();
    let mut buf = vec![0u8; STDOUT_CHUNK_SIZE];

    let read_result: anyhow::Result<()> = loop {
        tokio::select! {
            result = async {
                if let Some(ref mut stdout) = stdout {
                    stdout.read(&mut buf).await
                } else {
                    Ok(0)
                }
            } => {
                match result {
                    Ok(0) => break Ok(()), // EOF
                    Ok(n) => raw_pcm.extend_from_slice(&buf[..n]),
                    Err(e) => break Err(anyhow::anyhow!("Piper stdout read error: {}", e)),
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                crate::audio::pipe::terminate_child(&mut child).await;
                // Cleanup partial output
                let _ = std::fs::remove_file(&output_path);
                anyhow::bail!(
                    "TTS generation timed out after {}s — process killed",
                    GENERATE_TTS_TIMEOUT_SECS
                );
            }
        }
    };

    if let Err(e) = read_result {
        // Child may still be running (stdout read error) — kill and reap.
        crate::audio::pipe::terminate_child(&mut child).await;
        let _ = std::fs::remove_file(&output_path);
        return Err(e);
    }

    // Wait for process to finish — the SAME absolute deadline (P2-4), so EOF
    // does not grant a fresh timeout budget.
    let wait_result = tokio::time::timeout_at(deadline, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            if !status.success() {
                let stderr_msg = match &stderr_drain {
                    Some(d) => d.text().await,
                    None => String::new(),
                };
                let _ = std::fs::remove_file(&output_path);
                anyhow::bail!(
                    "Piper TTS failed (exit {}): {}",
                    status.code().unwrap_or(-1),
                    stderr_msg.trim()
                );
            }
        }
        Ok(Err(e)) => {
            // Child state is uncertain after a wait error — terminate and
            // reap explicitly before propagating (P2-2); kill_on_drop stays
            // only as defense-in-depth.
            crate::audio::pipe::terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&output_path);
            anyhow::bail!("TTS process wait error: {}", e);
        }
        Err(_) => {
            // Timeout waiting for child to exit — force kill and reap.
            crate::audio::pipe::terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&output_path);
            anyhow::bail!(
                "TTS generation timed out after {}s — process killed",
                GENERATE_TTS_TIMEOUT_SECS
            );
        }
    }

    // Piper outputs raw PCM (16-bit signed, mono, PIPER_SAMPLE_RATE Hz).
    // Wrap it in a proper WAV file using hound.
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: PIPER_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(&output_path, spec)?;

    // Convert raw bytes to i16 samples
    for chunk in raw_pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        writer.write_sample(sample)?;
    }

    writer.finalize()?;

    Ok(())
}

/// Generate TTS using paths resolved by `AppPaths`
pub async fn generate_tts_with_paths(
    text: &str,
    output_path: PathBuf,
    paths: &crate::paths::AppPaths,
) -> anyhow::Result<()> {
    let tools = crate::paths::resolve_tools(&paths.tool_dir);
    let piper_bin = tools
        .piper_bin
        .ok_or_else(|| anyhow::anyhow!("Piper binary not found"))?;
    let piper_model = tools
        .piper_model
        .ok_or_else(|| anyhow::anyhow!("Piper model not found"))?;

    // Validate paths exist before spawning process
    if !piper_bin.exists() {
        anyhow::bail!("Piper binary not found at: {}", piper_bin.display());
    }
    if !piper_model.exists() {
        anyhow::bail!("Piper model not found at: {}", piper_model.display());
    }

    // Ensure output directory exists
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let piper_str = piper_bin.to_string_lossy().to_string();
    let model_str = piper_model.to_string_lossy().to_string();

    generate_tts(text, output_path, Some(&piper_str), Some(&model_str)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-5: an injected asynchronous playback error becomes an authoritative
    /// Err — elapsed duration is NOT treated as proof of success.
    #[test]
    fn await_playback_surfaces_latched_output_error() {
        let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        *err.lock().unwrap() = Some("Output stream error: simulated device failure".to_string());
        let stop = Arc::new(AtomicBool::new(false));
        let position = std::sync::atomic::AtomicUsize::new(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

        let result = await_playback(&err, Some(&stop), deadline, &position, 1000);
        assert!(result.is_err(), "latched output error must fail playback");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("simulated device failure"),
            "error message must surface, got: {}",
            msg
        );
    }

    /// P2-3: playback completes successfully only when the output callback has
    /// consumed every sample — actual sample progress, not elapsed duration.
    #[test]
    fn await_playback_succeeds_once_position_reaches_total() {
        let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        let stop = Arc::new(AtomicBool::new(false));
        let position = std::sync::atomic::AtomicUsize::new(44100); // fully consumed
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

        let result = await_playback(&err, Some(&stop), deadline, &position, 44100);
        assert!(result.is_ok(), "position >= total must be success");
    }

    /// P2-3: a stalled output device (no sample progress) until the absolute
    /// deadline is an error, never a success.
    #[test]
    fn await_playback_errors_when_position_stalls_until_deadline() {
        let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        let stop = Arc::new(AtomicBool::new(false));
        let position = std::sync::atomic::AtomicUsize::new(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);

        let result = await_playback(&err, Some(&stop), deadline, &position, 44100);
        assert!(result.is_err(), "stalled playback must time out");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timed out"),
            "timeout error must be explicit, got: {}",
            msg
        );
    }

    /// P1-5: cancellation (stop flag) returns Ok without an error.
    #[test]
    fn await_playback_returns_ok_on_stop() {
        let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        let stop = Arc::new(AtomicBool::new(true));
        let position = std::sync::atomic::AtomicUsize::new(0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

        let result = await_playback(&err, Some(&stop), deadline, &position, 1000);
        assert!(result.is_ok());
    }
}
