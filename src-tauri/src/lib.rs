pub mod audio;

use tokio::sync::mpsc;

#[tauri::command]
async fn start_recording(
    output_path: String,
    sample_rate: Option<u32>,
) -> Result<String, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let sr = sample_rate.unwrap_or(44100);

    let path = std::path::PathBuf::from(&output_path);
    let path_clone = path.clone();

    tokio::spawn(async move {
        if let Err(e) = audio::capture::record_to_wav(path_clone, sr, 1, tx).await {
            eprintln!("Recording error: {}", e);
        }
    });

    // Wait for completion or error
    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped { file_path, duration_ms } => {
                return Ok(format!("Recording saved: {} ({}ms)", file_path, duration_ms));
            }
            audio::capture::CaptureEvent::Error { message } => {
                return Err(message);
            }
            _ => continue,
        }
    }

    Err("Recording interrupted".to_string())
}

#[tauri::command]
async fn stop_recording() -> Result<String, String> {
    // TODO: Signal recording to stop
    Ok("Stop signal sent".to_string())
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
    audio::playback::generate_tts(&text, path, None)
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
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
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
