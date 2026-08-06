use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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

        let (sample_tx, sample_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);

        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
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
        let mut chunks_since_flush = 0u32;
        const FLUSH_INTERVAL: u32 = 50;

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
pub async fn record_test_clip(
    sample_rate: u32,
    channels: u16,
    duration_secs: u32,
    temp_dir: PathBuf,
    event_tx: mpsc::Sender<CaptureEvent>,
) -> anyhow::Result<PathBuf> {
    let tmp_path = temp_dir.join(format!("device_test_{}.wav", std::process::id()));
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
        let (sample_tx, sample_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);

        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                let _ = sample_tx.send(data.to_vec());
            },
            move |err| {
                let _ = err_tx.try_send(CaptureEvent::Error {
                    message: format!("Test clip error: {}", err),
                });
            },
            None,
        )?;

        stream.play()?;
        let _ = event_tx.try_send(CaptureEvent::Started { sample_rate });

        let total_needed = sample_rate as u64 * duration_secs as u64;
        let mut total_frames = 0u64;

        while total_frames < total_needed {
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
                    total_frames += samples.len() as u64 / channels as u64;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        drop(stream);
        writer.finalize()?;

        let duration_ms = (total_frames * 1000) / sample_rate as u64;
        let _ = event_tx.try_send(CaptureEvent::Stopped {
            file_path: path_clone.to_string_lossy().to_string(),
            duration_ms,
        });

        Ok(())
    })
    .await??;

    // Verify the file was created and has content
    let metadata = std::fs::metadata(&tmp_path)?;
    if metadata.len() < 100 {
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
}
