pub mod audio;

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tauri::State;

/// Holds the current recording handle so stop_recording can cancel it
struct RecordingState {
    handle: Mutex<Option<audio::capture::RecordingHandle>>,
}

/// Structured return type for recording results
#[derive(serde::Serialize)]
struct RecordingResult {
    path: String,
    duration_ms: u64,
}

#[tauri::command]
async fn start_recording(
    output_path: String,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
) -> Result<RecordingResult, String> {
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

    // Spawn recording on a blocking thread using the shared capture function
    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    tokio::spawn(async move {
        if let Err(e) = audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await {
            eprintln!("Recording error: {}", e);
        }
    });

    // Wait for completion or error
    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped { file_path, duration_ms } => {
                // Clear the stored handle
                let mut guard = state.handle.lock().await;
                *guard = None;
                return Ok(RecordingResult { path: file_path, duration_ms });
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
    if text.trim().is_empty() {
        return Err("Text cannot be empty".to_string());
    }
    if text.len() > 10_000 {
        return Err("Text too long (max 10,000 characters)".to_string());
    }

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
