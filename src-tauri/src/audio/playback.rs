use crate::audio::pipe::StderrDrain;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Piper outputs raw PCM at 22050 Hz mono — tied to the en_US-amy-medium model
const PIPER_SAMPLE_RATE: u32 = 22050;

/// Maximum time allowed for a single playback operation before it is cancelled.
const PLAYBACK_TIMEOUT_SECS: u64 = 120;

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
                let _ = err_tx.try_send(PlaybackEvent::Error {
                    message: format!("Playback error: {}", err),
                });
            },
            None,
        )?;

        stream.play()?;

        // Wait for playback to finish, with cancellation and timeout
        let total_samples = samples.len();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(PLAYBACK_TIMEOUT_SECS);
        loop {
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

    // Send text via stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(text.as_bytes()).await?;
        drop(stdin); // close stdin to signal EOF
    }

    // Read stdout with async bounded reads and deadline enforcement
    let mut raw_pcm = Vec::new();
    let mut stdout = child.stdout.take();
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(GENERATE_TTS_TIMEOUT_SECS);
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
                let _ = child.kill().await;
                let _ = child.wait().await;
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
        let _ = std::fs::remove_file(&output_path);
        return Err(e);
    }

    // Wait for process to finish — with deadline
    let wait_result = timeout(
        tokio::time::Duration::from_secs(GENERATE_TTS_TIMEOUT_SECS),
        child.wait(),
    )
    .await;

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
            let _ = std::fs::remove_file(&output_path);
            anyhow::bail!("TTS process wait error: {}", e);
        }
        Err(_) => {
            // Timeout waiting for child to exit — force kill
            let _ = child.kill().await;
            let _ = child.wait().await;
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
