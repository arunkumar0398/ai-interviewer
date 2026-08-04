use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

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

/// Start recording from the default microphone to a WAV file using cpal (WASAPI on Windows).
/// Accepts an external stop flag so the caller controls when to stop.
pub async fn record_to_wav(
    output_path: PathBuf,
    sample_rate: u32,
    channels: u16,
    event_tx: mpsc::Sender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let sr = sample_rate;
    let ch = channels;

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
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

        let mut writer = hound::WavWriter::create(&output_path, spec)?;

        let (sample_tx, sample_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);

        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                // TODO: Heap alloc per callback (fine for MVP). Future: ring buffer for zero-copy.
                let _ = sample_tx.send(data.to_vec());
            },
            move |err| {
                eprintln!("Input stream error: {}", err);
                let _ = err_tx.try_send(CaptureEvent::Error {
                    message: format!("Audio stream error: {}", err),
                });
            },
            None,
        )?;

        stream.play()?;
        let _ = event_tx.try_send(CaptureEvent::Started { sample_rate: sr });

        let mut total_frames = 0u64;

        while !stop_flag.load(Ordering::SeqCst) {
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
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        drop(stream);
        writer.finalize()?;

        let duration_ms = (total_frames * 1000) / sr as u64;
        let _ = event_tx.try_send(CaptureEvent::Stopped {
            file_path: output_path.to_string_lossy().to_string(),
            duration_ms,
        });

        Ok(())
    })
    .await??;

    Ok(())
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
