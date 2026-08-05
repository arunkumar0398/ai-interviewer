pub mod audio;
pub mod db;
pub mod interview;
pub mod paths;

use std::sync::Arc;
use tauri::{Manager, State};
use tokio::sync::{mpsc, Mutex};

use paths::{get_app_config, resolve_app_paths, PathsState};

/// Holds the current recording handle so stop_recording can cancel it
struct RecordingState {
    handle: Mutex<Option<audio::capture::RecordingHandle>>,
}

/// Holds the database connection
struct DbState {
    db: Mutex<Option<db::Database>>,
}

/// Structured return type for recording results
#[derive(serde::Serialize)]
struct RecordingResult {
    path: String,
    duration_ms: u64,
}

#[tauri::command]
async fn start_recording(
    filename: String,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<RecordingResult, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let sr = sample_rate.unwrap_or(16000);
    let path = paths.paths.recordings_dir.join(&filename);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };

    {
        let mut guard = state.handle.lock().await;
        *guard = Some(handle);
    }

    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    tokio::spawn(async move {
        if let Err(e) = audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await
        {
            eprintln!("Recording error: {}", e);
        }
    });

    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped {
                file_path,
                duration_ms,
            } => {
                let mut guard = state.handle.lock().await;
                *guard = None;
                return Ok(RecordingResult {
                    path: file_path,
                    duration_ms,
                });
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
async fn generate_tts(
    text: String,
    output_path: String,
    paths: State<'_, PathsState>,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("Text cannot be empty".to_string());
    }
    if text.len() > 10_000 {
        return Err("Text too long (max 10,000 characters)".to_string());
    }

    let path = std::path::PathBuf::from(&output_path);
    audio::playback::generate_tts_with_paths(&text, path, &paths.paths)
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

// --- Phase 2 Commands ---

#[tauri::command]
async fn check_audio_devices() -> interview::device_check::DeviceCheckResult {
    let (tx, _rx) = mpsc::channel(32);
    interview::device_check::run_device_check(tx).await
}

/// Result of a single interview round
#[derive(serde::Serialize)]
struct InterviewRoundResult {
    metadata: interview::orchestrator::AudioMetadata,
    transcription: String,
}

#[tauri::command]
async fn run_interview_round(
    question: String,
    round_index: usize,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<InterviewRoundResult, String> {
    let output_dir = paths.paths.recordings_dir.clone();
    std::fs::create_dir_all(&output_dir).map_err(|e| e.to_string())?;

    let (event_tx, _event_rx) = mpsc::channel(32);
    let (tts_event_tx, _tts_event_rx) = mpsc::channel(32);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    {
        let recording_handle = audio::capture::RecordingHandle {
            stop: stop_flag.clone(),
        };
        let mut guard = state.handle.lock().await;
        *guard = Some(recording_handle);
    }

    let paths_clone = paths.paths.clone();
    let output_clone = output_dir.clone();
    let stop_clone = stop_flag.clone();
    let event_tx_clone = event_tx.clone();
    let tts_event_tx_clone = tts_event_tx.clone();

    let result = tokio::spawn(async move {
        interview::orchestrator::run_interview_round(
            &question,
            &paths_clone,
            &output_clone,
            round_index,
            event_tx_clone,
            tts_event_tx_clone,
            stop_clone,
        )
        .await
    })
    .await
    .map_err(|e| e.to_string())?;

    {
        let mut guard = state.handle.lock().await;
        *guard = None;
    }

    match result {
        Ok((metadata, transcription)) => Ok(InterviewRoundResult {
            metadata,
            transcription,
        }),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn stop_interview_round(state: State<'_, Arc<RecordingState>>) -> Result<String, String> {
    let guard = state.handle.lock().await;
    match &*guard {
        Some(handle) => {
            handle.stop();
            Ok("Stop signal sent".to_string())
        }
        None => Err("No active interview round".to_string()),
    }
}

#[tauri::command]
fn verify_tools_installation(paths: State<'_, PathsState>) -> serde_json::Value {
    let p = &paths.paths;
    serde_json::json!({
        "piper": p.piper_bin.exists(),
        "whisper": p.whisper_bin.exists(),
        "model": p.whisper_model.exists(),
    })
}

// --- Tools Commands ---

/// Return the resolved tool directory.  Prefer `get_app_config` instead.
#[tauri::command]
fn get_tools_dir(paths: State<'_, PathsState>) -> String {
    paths.paths.tool_dir.to_string_lossy().to_string()
}

// --- Database Commands ---

#[tauri::command]
async fn create_session(
    session_id: String,
    candidate_name: String,
    state: State<'_, Arc<DbState>>,
) -> Result<(), String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.create_session(&session_id, &candidate_name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn insert_round(
    session_id: String,
    round_index: i32,
    question: String,
    transcription: String,
    audio_path: String,
    sha256: String,
    duration_ms: u64,
    sample_rate: u32,
    channels: u16,
    file_size_bytes: u64,
    state: State<'_, Arc<DbState>>,
) -> Result<i64, String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.insert_round(
        &session_id,
        round_index,
        &question,
        &transcription,
        &audio_path,
        &sha256,
        duration_ms,
        sample_rate,
        channels,
        file_size_bytes,
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn complete_session(
    session_id: String,
    total_rounds: i32,
    state: State<'_, Arc<DbState>>,
) -> Result<(), String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.complete_session(&session_id, total_rounds)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_sessions(state: State<'_, Arc<DbState>>) -> Result<Vec<db::InterviewSession>, String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.get_sessions().map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_rounds(
    session_id: String,
    state: State<'_, Arc<DbState>>,
) -> Result<Vec<db::InterviewRound>, String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.get_rounds(&session_id).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // 1. Resolve all filesystem paths
            let paths = resolve_app_paths(app.handle())?;

            // 2. Initialize database at startup
            let database = db::Database::open(&paths.db_path).map_err(|e| {
                eprintln!("[startup] Database init failed: {}", e);
                e
            })?;

            // 3. Manage all states
            app.manage(PathsState { paths });
            app.manage(Arc::new(RecordingState {
                handle: Mutex::new(None),
            }));
            app.manage(Arc::new(DbState {
                db: Mutex::new(Some(database)),
            }));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_config,
            start_recording,
            stop_recording,
            play_audio,
            generate_tts,
            list_audio_devices,
            check_audio_devices,
            run_interview_round,
            stop_interview_round,
            verify_tools_installation,
            get_tools_dir,
            create_session,
            insert_round,
            complete_session,
            get_sessions,
            get_rounds,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
