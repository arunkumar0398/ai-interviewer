pub mod audio;

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tauri::State;

/// Holds the current recording handle so stop_recording can cancel it
struct RecordingState {
    handle: Mutex<Option<audio::RecordingHandle>>,
}

#[tauri::command]
async fn start_recording(
    output_path: String,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let sr = sample_rate.unwrap_or(16000); // 16kHz for whisper compatibility
    let path = std::path::PathBuf::from(&output_path);

    // Create a stop handle
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle { stop: stop_flag.clone() };

    // Store the handle so stop_recording can access it
    {
        let mut guard = state.handle.lock().await;
        *guard = Some(handle);
    }

    // Spawn recording on a blocking thread
    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    tokio::spawn(async move {
        // We need to call the capture function directly with our stop flag
        match record_with_handle(path_clone, sr, 1, event_tx, stop_clone).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Recording error: {}", e);
            }
        }
    });

    // Wait for completion or error
    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped { file_path, duration_ms } => {
                // Clear the stored handle
                let mut guard = state.handle.lock().await;
                *guard = None;
                return Ok(format!("Recording saved: {} ({}ms)", file_path, duration_ms));
            }
            audio::capture::CaptureEvent::Error { message } => {
                let mut guard = state.handle.lock().await;
                *guard = None;
                return Err(message);
            }
            _ => continue,
        }
    }

    let mut guard = state.handle.lock().await;
    *guard = None;
    Err("Recording interrupted".to_string())
}

/// Internal recording function with explicit stop flag
async fn record_with_handle(
    output_path: std::path::PathBuf,
    sample_rate: u32,
    channels: u16,
    event_tx: mpsc::Sender<audio::capture::CaptureEvent>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use hound::{WavSpec, WavWriter};

        let host = cpal::default_host();

        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device found"))?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = WavWriter::create(&output_path, spec)?;

        let (sample_tx, sample_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);

        let err_tx = event_tx.clone();
        let stream = device.build_input_stream(
            &config,
            move |data: &[f32], _info: &cpal::InputCallbackInfo| {
                let _ = sample_tx.send(data.to_vec());
            },
            move |err| {
                eprintln!("Input stream error: {}", err);
                let _ = err_tx.try_send(audio::capture::CaptureEvent::Error {
                    message: format!("Audio stream error: {}", err),
                });
            },
            None,
        )?;

        stream.play()?;
        let _ = event_tx.try_send(audio::capture::CaptureEvent::Started { sample_rate });

        let mut total_frames = 0u64;

        while !stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
            match sample_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(samples) => {
                    let rms = if samples.is_empty() {
                        0.0
                    } else {
                        let sum: f32 = samples.iter().map(|s| s * s).sum();
                        (sum / samples.len() as f32).sqrt()
                    };
                    let _ = event_tx.try_send(audio::capture::CaptureEvent::Level { rms });

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
        let _ = event_tx.try_send(audio::capture::CaptureEvent::Stopped {
            file_path: output_path.to_string_lossy().to_string(),
            duration_ms,
        });

        Ok(())
    })
    .await?
}

#[tauri::command]
async fn stop_recording(state: State<'_, Arc<RecordingState>>) -> Result<String, String> {
    let guard = state.handle.lock().await;
    match &*guard {
        Some(handle) => {
            handle.stop();
            Ok("Stop signal sent".to_string())
        }
        None => Err("No active recording".to_string()),
    }
}

#[tauri::command]
async fn play_audio(file_path: String) -> Result<String, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let path = std::path::PathBuf::from(&file_path);

    tokio::spawn(async move {
        if let Err(e) = audio::playback::play_wav(path, tx).await {
            eprintln!("Playback error: {}", e);
        }
    });

    while let Some(event) = rx.recv().await {
        match event {
            audio::playback::PlaybackEvent::Completed => {
                return Ok("Playback completed".to_string());
            }
            audio::playback::PlaybackEvent::Error { message } => {
                return Err(message);
            }
            _ => continue,
        }
    }

    Err("Playback interrupted".to_string())
}

#[tauri::command]
async fn generate_tts(text: String, output_path: String) -> Result<String, String> {
    let path = std::path::PathBuf::from(&output_path);
    audio::playback::generate_tts_with_paths(&text, path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(output_path)
}

#[tauri::command]
async fn list_audio_devices() -> Result<Vec<String>, String> {
    audio::capture::list_input_devices()
        .await
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let recording_state = Arc::new(RecordingState {
        handle: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(recording_state)
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            play_audio,
            generate_tts,
            list_audio_devices,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
