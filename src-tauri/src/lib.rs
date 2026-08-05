pub mod audio;
pub mod db;
pub mod interview;

/// Single source of truth for external tools directory
const TOOLS_DIR: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools";

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tauri::{Manager, State};

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
    output_path: String,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
) -> Result<RecordingResult, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let sr = sample_rate.unwrap_or(16000);
    let path = std::path::PathBuf::from(&output_path);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle { stop: stop_flag.clone() };

    {
        let mut guard = state.handle.lock().await;
        *guard = Some(handle);
    }

    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    tokio::spawn(async move {
        if let Err(e) = audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await {
            eprintln!("Recording error: {}", e);
        }
    });

    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped { file_path, duration_ms } => {
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
    audio::playback::generate_tts_with_paths(&text, path, TOOLS_DIR)
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
async fn check_audio_devices() -> Result<interview::device_check::DeviceCheckResult, String> {
    let (tx, _rx) = mpsc::channel(32);
    interview::device_check::run_device_check(tx)
        .await
        .pipe_into(Ok)
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
    tools_dir: String,
    output_dir: String,
    round_index: usize,
    state: State<'_, Arc<RecordingState>>,
) -> Result<InterviewRoundResult, String> {
    let tools = std::path::PathBuf::from(&tools_dir);
    let output = std::path::PathBuf::from(&output_dir);

    // Create output directory if it doesn't exist
    std::fs::create_dir_all(&output).map_err(|e| e.to_string())?;

    let (event_tx, _event_rx) = mpsc::channel(32);
    let (tts_event_tx, _tts_event_rx) = mpsc::channel(32);

    // Shared stop flag for this round
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Store a handle so the frontend can cancel
    {
        let recording_handle = audio::capture::RecordingHandle {
            stop: stop_flag.clone(),
        };
        let mut guard = state.handle.lock().await;
        *guard = Some(recording_handle);
    }

    let tools_clone = tools.clone();
    let output_clone = output.clone();
    let stop_clone = stop_flag.clone();
    let event_tx_clone = event_tx.clone();
    let tts_event_tx_clone = tts_event_tx.clone();

    let result = tokio::spawn(async move {
        interview::orchestrator::run_interview_round(
            &question,
            &tools_clone,
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

    // Clear the handle
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
async fn verify_tools_installation(tools_dir: String) -> Result<serde_json::Value, String> {
    let tools = std::path::PathBuf::from(&tools_dir);

    let piper_ok = audio::tts_supervisor::verify_piper_installation(&tools).is_ok();
    let whisper_ok = tools.join("whisper").join("Release").join("main.exe").exists();
    let model_ok = tools
        .join("models")
        .join("ggml-tiny.en.bin")
        .exists();

    Ok(serde_json::json!({
        "piper": piper_ok,
        "whisper": whisper_ok,
        "model": model_ok,
    }))
}

// --- Tools Commands ---

#[tauri::command]
fn get_tools_dir() -> String {
    TOOLS_DIR.to_string()
}

// --- Database Commands ---

#[tauri::command]
async fn get_app_dir(app: tauri::AppHandle) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
async fn init_database(db_path: String, state: State<'_, Arc<DbState>>) -> Result<String, String> {
    let path = std::path::PathBuf::from(&db_path);
    let database = db::Database::open(&path).map_err(|e| e.to_string())?;
    let mut guard = state.db.lock().await;
    *guard = Some(database);
    Ok(db_path)
}

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
    let recording_state = Arc::new(RecordingState {
        handle: Mutex::new(None),
    });

    let db_state = Arc::new(DbState {
        db: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(recording_state)
        .manage(db_state)
        .invoke_handler(tauri::generate_handler![
            start_recording,
            stop_recording,
            play_audio,
            generate_tts,
            list_audio_devices,
            check_audio_devices,
            run_interview_round,
            stop_interview_round,
            verify_tools_installation,
            get_app_dir,
            get_tools_dir,
            init_database,
            create_session,
            insert_round,
            complete_session,
            get_sessions,
            get_rounds,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Extension trait for chaining
trait PipeInto<T> {
    fn pipe_into<F, R>(self, f: F) -> R
    where
        F: FnOnce(T) -> R;
}

impl<T> PipeInto<T> for T {
    fn pipe_into<F, R>(self, f: F) -> R
    where
        F: FnOnce(T) -> R,
    {
        f(self)
    }
}
