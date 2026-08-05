use std::path::PathBuf;
use std::process::{Command, Stdio};
use tokio::sync::mpsc;

/// Piper outputs raw PCM at 22050 Hz mono — tied to the en_US-amy-medium model
const PIPER_SAMPLE_RATE: u32 = 22050;

/// Playback events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum PlaybackEvent {
    Started { duration_ms: u64 },
    Completed,
    Error { message: String },
}

/// Play a WAV file through the system speakers using cpal
pub async fn play_wav(
    file_path: PathBuf,
    event_tx: mpsc::Sender<PlaybackEvent>,
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

        // Wait for playback to finish
        // TODO: Replace busy-poll with tokio::sync::Notify for cleaner async wakeup
        let total_samples = samples.len();
        loop {
            let current = pos.load(std::sync::atomic::Ordering::Relaxed);
            if current >= total_samples {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        drop(stream);
        let _ = event_tx.try_send(PlaybackEvent::Completed);

        Ok(())
    })
    .await?
}

/// Generate a TTS WAV file using Piper via stdin (correct invocation per spike findings)
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
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Send text via stdin
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(text.as_bytes())?;
        drop(stdin); // close stdin to signal EOF
    }

    let output = child.wait_with_output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Piper TTS failed: {}", stderr);
    }

    // Piper outputs raw PCM (16-bit signed, mono, PIPER_SAMPLE_RATE Hz).
    // Wrap it in a proper WAV file using hound.
    let raw_pcm = output.stdout;
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
    if !paths.piper_bin.exists() {
        anyhow::bail!("Piper binary not found at: {}", paths.piper_bin.display());
    }
    if !paths.piper_model.exists() {
        anyhow::bail!("Piper model not found at: {}", paths.piper_model.display());
    }

    let piper_str = paths.piper_bin.to_string_lossy().to_string();
    let model_str = paths.piper_model.to_string_lossy().to_string();

    generate_tts(text, output_path, Some(&piper_str), Some(&model_str)).await
}
