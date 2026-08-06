pub mod audio;
pub mod db;
pub mod interview;
pub mod paths;

use std::sync::Arc;
use tauri::{Manager, State};
use tokio::sync::{mpsc, Mutex};

use paths::{get_app_config, resolve_app_paths, uuid_to_path, PathsState};

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

/// Atomically acquire the recording slot. Returns Err if already active.
async fn acquire_recording(
    state: &RecordingState,
    handle: audio::capture::RecordingHandle,
) -> Result<(), String> {
    let mut guard = state.handle.lock().await;
    if guard.is_some() {
        return Err("A recording is already active".to_string());
    }
    *guard = Some(handle);
    Ok(())
}

/// Clear the recording slot unconditionally.
async fn clear_active_recording(state: &RecordingState) {
    let mut guard = state.handle.lock().await;
    *guard = None;
}

#[tauri::command]
async fn start_recording(
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<RecordingResult, String> {
    let (tx, mut rx) = mpsc::channel(32);
    let sr = sample_rate.unwrap_or(16000);

    // Backend generates path: recordings/<session_id>/<round_id>.wav
    let session_dir = paths.paths.session_recordings_dir(session_id);
    std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
    let path = session_dir.join(format!("{}.wav", uuid_to_path(&round_id)));

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };

    // Atomic acquire — single lock, check-and-set
    acquire_recording(&state, handle).await?;

    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    // Spawn worker; drop original tx so channel closes when worker finishes
    let worker = tokio::spawn(async move {
        if let Err(e) = audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await
        {
            eprintln!("Recording error: {}", e);
        }
    });
    drop(tx);

    // Wait for worker completion or channel events
    let join_result = worker.await;
    clear_active_recording(&state).await;

    // Check for join failure (panic/cancellation)
    join_result.map_err(|e| format!("Recording worker failed: {}", e))?;

    // Drain remaining events from channel
    while let Some(event) = rx.recv().await {
        match event {
            audio::capture::CaptureEvent::Stopped {
                file_path,
                duration_ms,
            } => {
                return Ok(RecordingResult {
                    path: file_path,
                    duration_ms,
                });
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
async fn play_round_audio(
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    paths: State<'_, PathsState>,
) -> Result<String, String> {
    let file_path = paths.paths.round_audio_path(session_id, round_id);

    if !file_path.exists() {
        return Err(format!(
            "No recording found for session {} round {}",
            session_id, round_id
        ));
    }

    let (tx, mut rx) = mpsc::channel(32);
    let worker_tx = tx.clone();
    let path = file_path;

    let worker = tokio::spawn(async move {
        if let Err(e) = audio::playback::play_wav(path, worker_tx).await {
            eprintln!("Playback error: {}", e);
        }
    });
    drop(tx);

    let join_result = worker.await;
    join_result.map_err(|e| format!("Playback worker failed: {}", e))?;

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

    Ok("Playback completed".to_string())
}

#[tauri::command]
async fn generate_tts(
    text: String,
    request_id: uuid::Uuid,
    paths: State<'_, PathsState>,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("Text cannot be empty".to_string());
    }
    if text.len() > 10_000 {
        return Err("Text too long (max 10,000 characters)".to_string());
    }

    let output_path = paths.paths.tts_output_path(request_id);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    audio::playback::generate_tts_with_paths(&text, output_path.clone(), &paths.paths)
        .await
        .map_err(|e| e.to_string())?;
    Ok(output_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn list_audio_devices() -> Result<Vec<String>, String> {
    audio::capture::list_input_devices()
        .await
        .map_err(|e| e.to_string())
}

// --- Phase 2 Commands ---

#[tauri::command]
async fn check_audio_devices(
    paths: State<'_, PathsState>,
) -> Result<interview::device_check::DeviceCheckResult, String> {
    let (tx, _rx) = mpsc::channel(32);
    Ok(interview::device_check::run_device_check(paths.paths.temp_dir.clone(), tx).await)
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
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<InterviewRoundResult, String> {
    let (event_tx, _event_rx) = mpsc::channel(32);
    let (tts_event_tx, _tts_event_rx) = mpsc::channel(32);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Atomic acquire — single lock, check-and-set
    let recording_handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };
    acquire_recording(&state, recording_handle).await?;

    let paths_clone = paths.paths.clone();
    let stop_clone = stop_flag.clone();
    let event_tx_clone = event_tx.clone();
    let tts_event_tx_clone = tts_event_tx.clone();

    // Spawn worker; guaranteed cleanup on all paths
    let worker = tokio::spawn(async move {
        interview::orchestrator::run_interview_round(
            &question,
            &paths_clone,
            session_id,
            round_id,
            event_tx_clone,
            tts_event_tx_clone,
            stop_clone,
        )
        .await
    });

    let join_result = worker.await;
    clear_active_recording(&state).await;

    let result = join_result.map_err(|e| format!("Interview worker failed: {}", e))?;

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

// --- Tools Commands ---

/// Return the resolved tool directory.  Prefer `get_app_config` instead.
#[tauri::command]
fn get_tools_dir(paths: State<'_, PathsState>) -> String {
    paths.paths.tool_dir.to_string_lossy().to_string()
}

// --- Database Commands ---

#[tauri::command]
async fn create_session(
    session_id: uuid::Uuid,
    candidate_name: String,
    state: State<'_, Arc<DbState>>,
) -> Result<(), String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.create_session(&session_id.hyphenated().to_string(), &candidate_name)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn insert_round(
    session_id: uuid::Uuid,
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
        &session_id.hyphenated().to_string(),
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
    session_id: uuid::Uuid,
    total_rounds: i32,
    state: State<'_, Arc<DbState>>,
) -> Result<(), String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.complete_session(&session_id.hyphenated().to_string(), total_rounds)
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
    session_id: uuid::Uuid,
    state: State<'_, Arc<DbState>>,
) -> Result<Vec<db::InterviewRound>, String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.get_rounds(&session_id.hyphenated().to_string())
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // 1. Resolve all filesystem paths
            let paths = resolve_app_paths(app.handle())?;

            // 2. Initialize database at startup
            let database = db::Database::open(&paths.paths.db_path).map_err(|e| {
                eprintln!("[startup] Database init failed: {}", e);
                e
            })?;

            // 3. Manage all states
            app.manage(paths);
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
            play_round_audio,
            generate_tts,
            list_audio_devices,
            check_audio_devices,
            run_interview_round,
            stop_interview_round,
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
