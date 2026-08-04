use std::path::PathBuf;
use tokio::sync::mpsc;

/// Playback events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum PlaybackEvent {
    Started { duration_ms: u64 },
    Completed,
    Error { message: String },
}

/// Play a WAV file through the system speakers
pub async fn play_wav(
    file_path: PathBuf,
    event_tx: mpsc::Sender<PlaybackEvent>,
) -> anyhow::Result<()> {
    // TODO: Replace with actual audio playback (rodio/Symphonia)
    // For now, simulate playback
    let info = crate::audio::wav::validate_wav(&file_path)?;

    let duration_ms = (info.data_size as u64 * 8 * 1000)
        / (info.sample_rate as u64 * info.channels as u64 * info.bits_per_sample as u64);

    let _ = event_tx.send(PlaybackEvent::Started { duration_ms }).await;

    // Simulate playback duration
    tokio::time::sleep(tokio::time::Duration::from_millis(duration_ms.min(5000))).await;

    let _ = event_tx.send(PlaybackEvent::Completed).await;
    Ok(())
}

/// Generate a TTS WAV file using Piper
pub async fn generate_tts(
    text: &str,
    output_path: PathBuf,
    piper_binary: Option<&str>,
) -> anyhow::Result<()> {
    let piper = piper_binary.unwrap_or("piper");

    // Check if piper is available
    let output = tokio::process::Command::new(piper)
        .arg("--version")
        .output()
        .await;

    match output {
        Ok(_) => {
            // Piper is available, generate TTS
            let output = tokio::process::Command::new(piper)
                .arg("--text")
                .arg(text)
                .arg("--output_file")
                .arg(&output_path)
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("Piper TTS failed: {}", stderr);
            }
        }
        Err(_) => {
            // Piper not found, create a placeholder silent WAV
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 22050,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::create(&output_path, spec)?;
            // 2 seconds of silence as placeholder
            let frames = 22050 * 2;
            for _ in 0..frames {
                writer.write_sample(0i16)?;
            }
            writer.finalize()?;
        }
    }

    Ok(())
}
