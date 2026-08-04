use std::path::PathBuf;
use tokio::sync::mpsc;

/// Audio capture events sent to the UI
#[derive(Debug, Clone, serde::Serialize)]
pub enum CaptureEvent {
    Started { sample_rate: u32 },
    Level { rms: f32 },
    Stopped { file_path: String, duration_ms: u64 },
    Error { message: String },
}

/// Start recording from the default microphone to a WAV file
pub async fn record_to_wav(
    output_path: PathBuf,
    sample_rate: u32,
    channels: u16,
    event_tx: mpsc::Sender<CaptureEvent>,
) -> anyhow::Result<()> {
    use hound::{WavSpec, WavWriter};

    let spec = WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(&output_path, spec)?;

    // TODO: Replace with actual native audio capture (CPAL/WASAPI)
    // For now, record silence as a placeholder for the spike
    let _ = event_tx.send(CaptureEvent::Started { sample_rate }).await;

    let frames_per_chunk = sample_rate as usize / 10; // 100ms chunks
    let total_chunks = 50; // 5 seconds
    let mut total_frames = 0u64;

    for _ in 0..total_chunks {
        // Write silence placeholder
        let silence = vec![0i16; frames_per_chunk * channels as usize];
        for &sample in &silence {
            writer.write_sample(sample)?;
        }
        total_frames += frames_per_chunk as u64;

        // Calculate RMS level (will be real audio in production)
        let rms = 0.0f32;
        let _ = event_tx.send(CaptureEvent::Level { rms }).await;
    }

    writer.finalize()?;

    let duration_ms = (total_frames * 1000) / sample_rate as u64;
    let _ = event_tx
        .send(CaptureEvent::Stopped {
            file_path: output_path.to_string_lossy().to_string(),
            duration_ms,
        })
        .await;

    Ok(())
}

/// List available audio input devices
pub async fn list_input_devices() -> anyhow::Result<Vec<String>> {
    // TODO: Implement with CPAL/WASAPI
    Ok(vec!["Default Microphone".to_string()])
}
