pub mod audio;
pub mod db;
pub mod interview;
pub mod paths;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
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

/// Scope guard that clears RecordingState when dropped. This is only a
/// panic/cancellation safety net: every controlled path clears RecordingState
/// explicitly (and disarms the guard) before returning, so the slot is free
/// deterministically — no spawned task, no timing dependence.
struct RecordingGuard {
    state: Arc<RecordingState>,
    armed: std::sync::atomic::AtomicBool,
}

impl RecordingGuard {
    /// Disarm the safety net after explicit cleanup has run, so Drop does not
    /// schedule a redundant async clear.
    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

impl Drop for RecordingGuard {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            let state = self.state.clone();
            // Spawn the async clear on the runtime; this runs even on panic.
            // clone() ensures the Arc keeps the state alive past the guard's lifetime.
            tokio::runtime::Handle::current().spawn(async move {
                clear_active_recording(&state).await;
            });
        }
    }
}

#[tauri::command]
async fn start_recording(
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    sample_rate: Option<u32>,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<RecordingResult, String> {
    let (tx, _rx) = mpsc::channel(32);
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
        audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await
    });
    drop(tx);

    // Wait for worker completion or channel events
    let join_result = worker.await;
    clear_active_recording(&state).await;

    // Check for join failure (panic/cancellation)
    let record_result = join_result
        .map_err(|e| format!("Recording worker failed: {}", e))?
        .map_err(|e| e.to_string())?;

    Ok(RecordingResult {
        path: record_result.file_path.to_string_lossy().to_string(),
        duration_ms: record_result.duration_ms,
    })
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

    let worker =
        tokio::spawn(async move { audio::playback::play_wav(path, worker_tx, None).await });
    drop(tx);

    let join_result = worker.await;
    join_result
        .map_err(|e| format!("Playback worker failed: {}", e))?
        .map_err(|e| e.to_string())?;

    while let Some(event) = rx.recv().await {
        match event {
            audio::playback::PlaybackEvent::Completed => {
                return Ok("Playback completed".to_string());
            }
            audio::playback::PlaybackEvent::Cancelled => {
                return Ok("Playback cancelled".to_string());
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
    // The inner generate_tts function owns process-level timeout enforcement
    // (kill, wait/reap, cleanup). This outer timeout is only defense-in-depth.
    let tts_timeout = std::time::Duration::from_secs(30);
    tokio::time::timeout(
        tts_timeout,
        audio::playback::generate_tts_with_paths(&text, output_path.clone(), &paths.paths),
    )
    .await
    .map_err(|_| "TTS generation timed out".to_string())?
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

/// Payload emitted to the frontend for interview phase transitions
#[derive(serde::Serialize, Clone)]
struct PhaseEventPayload {
    phase: String,
    question: Option<String>,
    duration_ms: Option<u64>,
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn run_interview_round(
    question: String,
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    round_index: i32,
    is_final: bool,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
    db_state: State<'_, Arc<DbState>>,
    app: tauri::AppHandle,
) -> Result<InterviewRoundResult, String> {
    let (event_tx, _event_rx) = mpsc::channel(32);
    let (tts_event_tx, _tts_event_rx) = mpsc::channel(32);
    let (phase_tx, mut phase_rx) = mpsc::channel(8);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Atomic acquire — single lock, check-and-set
    let recording_handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };
    acquire_recording(&state, recording_handle).await?;

    // Safety-net guard: clears RecordingState on any early return, panic, or
    // cancellation. Every controlled path below clears RecordingState
    // explicitly (after the worker has fully terminated) and disarms it.
    let recording_guard = RecordingGuard {
        state: state.inner().clone(),
        armed: std::sync::atomic::AtomicBool::new(true),
    };

    let paths_clone = paths.paths.clone();
    let stop_clone = stop_flag.clone();
    let event_tx_clone = event_tx.clone();
    let tts_event_tx_clone = tts_event_tx.clone();

    // Spawn phase event relay: forwards phase changes to Tauri event system
    let app_clone = app.clone();
    let phase_relay = tokio::spawn(async move {
        while let Some(phase) = phase_rx.recv().await {
            let payload = match &phase {
                interview::orchestrator::InterviewPhase::SpeakingQuestion { question } => {
                    PhaseEventPayload {
                        phase: "speaking-question".to_string(),
                        question: Some(question.clone()),
                        duration_ms: None,
                    }
                }
                interview::orchestrator::InterviewPhase::Settling { duration_ms } => {
                    PhaseEventPayload {
                        phase: "settling".to_string(),
                        question: None,
                        duration_ms: Some(*duration_ms),
                    }
                }
                interview::orchestrator::InterviewPhase::RecordingAnswer => PhaseEventPayload {
                    phase: "recording-answer".to_string(),
                    question: None,
                    duration_ms: None,
                },
                interview::orchestrator::InterviewPhase::Processing => PhaseEventPayload {
                    phase: "processing".to_string(),
                    question: None,
                    duration_ms: None,
                },
                interview::orchestrator::InterviewPhase::Complete => PhaseEventPayload {
                    phase: "complete".to_string(),
                    question: None,
                    duration_ms: None,
                },
                interview::orchestrator::InterviewPhase::Idle => PhaseEventPayload {
                    phase: "idle".to_string(),
                    question: None,
                    duration_ms: None,
                },
                interview::orchestrator::InterviewPhase::Error { message } => PhaseEventPayload {
                    phase: "error".to_string(),
                    question: Some(message.clone()),
                    duration_ms: None,
                },
            };
            let _ = app_clone.emit("interview-phase", &payload);
        }
    });

    // Spawn worker; guaranteed cleanup on all paths
    let question_clone = question.clone();
    let worker = tokio::spawn(async move {
        interview::orchestrator::run_interview_round(
            &question_clone,
            &paths_clone,
            session_id,
            round_id,
            event_tx_clone,
            tts_event_tx_clone,
            stop_clone,
            Some(phase_tx),
        )
        .await
    });

    // Full-round deadline + cooperative shutdown
    const ROUND_TIMEOUT_SECS: u64 = 300;
    const GRACE_SECS: u64 = 5;

    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(ROUND_TIMEOUT_SECS);

    let mut worker_future = std::pin::pin!(worker);

    // The JoinHandle result is captured directly from the select and processed
    // exactly once. A completed JoinHandle must never be polled again (polling
    // a completed handle consumes/panics), so the result is bound here rather
    // than discarded with `_` and re-awaited afterwards.
    //   Ok(join_result) => worker finished before the deadline (success, error, or panic)
    //   Err(_)          => the 300s deadline expired first
    let outcome = tokio::select! {
        result = &mut worker_future => Ok(result),
        _ = tokio::time::sleep_until(deadline) => Err(ROUND_TIMEOUT_SECS),
    };

    match outcome {
        Ok(join_result) => {
            // Normal completion path — process the JoinHandle result exactly once.
            phase_relay.abort();

            let result = match join_result {
                Ok(inner) => inner.map_err(|e| e.to_string()),
                Err(join_err) => Err(format!("Interview worker failed: {}", join_err)),
            };

            // Worker has fully terminated — release the recording slot
            // deterministically and disarm the safety net.
            clear_active_recording(&state).await;
            recording_guard.disarm();

            match result {
                Ok((metadata, transcription)) => {
                    // Persist round atomically: round INSERT + session
                    // total_rounds increment + completed_at (when final) commit
                    // in ONE transaction.
                    let persist_result: Result<i64, String> = (async {
                        let guard = db_state.db.lock().await;
                        let db = guard
                            .as_ref()
                            .ok_or_else(|| "Database not initialized".to_string())?;
                        db.insert_round_with_session_update(
                            &session_id.hyphenated().to_string(),
                            round_index,
                            &question,
                            &transcription,
                            &metadata.file_path,
                            &metadata.sha256,
                            metadata.duration_ms,
                            metadata.sample_rate,
                            metadata.channels,
                            metadata.file_size_bytes,
                            is_final,
                        )
                        .map_err(|e| {
                            let msg = e.to_string();
                            if msg.contains("UNIQUE constraint failed") {
                                format!("Round {} already exists for this session", round_index + 1)
                            } else {
                                format!("Failed to persist round: {}", e)
                            }
                        })
                    })
                    .await;

                    match persist_result {
                        Ok(_) => Ok(InterviewRoundResult {
                            metadata,
                            transcription,
                        }),
                        Err(e) => {
                            // The round never committed — the WAV was provisional
                            // until this transaction. Delete it plus any partial
                            // temp artifacts for this attempted round.
                            let _ = std::fs::remove_file(&metadata.file_path);
                            Err(e)
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            }
        }
        Err(_) => {
            // Deadline hit — signal cooperative cancellation via stop_flag.
            stop_flag.store(true, Ordering::SeqCst);
            let _ = app.emit(
                "interview-phase",
                PhaseEventPayload {
                    phase: "error".to_string(),
                    question: Some(format!("Round timed out after {}s", ROUND_TIMEOUT_SECS)),
                    duration_ms: None,
                },
            );

            // Await the SAME worker up to the 5s grace period — allows cooperative
            // exit. If it finishes in time, its result is captured (and discarded:
            // the round timed out regardless).
            let graceful = tokio::select! {
                result = &mut worker_future => Some(result),
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(GRACE_SECS)) => None,
            };

            if graceful.is_none() {
                // Grace period expired — force abort, then await the aborted
                // JoinHandle so worker lifecycle is fully resolved before this
                // command returns.
                worker_future.as_mut().abort();
                let _ = worker_future.await;
            }

            // Worker has fully terminated (cooperatively or aborted) — release
            // the recording slot deterministically and disarm the safety net.
            clear_active_recording(&state).await;
            recording_guard.disarm();

            // The round never committed — remove provisional artifacts: the
            // WAV for this round_id, its partial temp file, and the session
            // temp transcript directory.
            let wav_path = paths.paths.round_audio_path(session_id, round_id);
            let _ = std::fs::remove_file(&wav_path);
            let _ = std::fs::remove_file(wav_path.with_extension("wav.tmp"));
            let temp_dir = paths
                .paths
                .temp_dir
                .join(session_id.hyphenated().to_string());
            let _ = std::fs::remove_dir_all(temp_dir);

            phase_relay.abort();
            Err(format!(
                "Round timed out after {}s — processes terminated, partial artifacts cleaned up",
                ROUND_TIMEOUT_SECS
            ))
        }
    }
}

/// Retry a failed interview round. Generates a new round_id, so the old
/// round (if partially written) is left in the DB as-is and the new one
/// is inserted under a fresh UUID via the UNIQUE(session_id, round_index)
/// constraint (INSERT OR IGNORE).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn retry_interview_round(
    question: String,
    session_id: uuid::Uuid,
    round_index: i32,
    is_final: bool,
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
    db_state: State<'_, Arc<DbState>>,
    app: tauri::AppHandle,
) -> Result<InterviewRoundResult, String> {
    let new_round_id = uuid::Uuid::new_v4();
    run_interview_round(
        question,
        session_id,
        new_round_id,
        round_index,
        is_final,
        state,
        paths,
        db_state,
        app,
    )
    .await
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
            retry_interview_round,
            stop_interview_round,
            get_tools_dir,
            create_session,
            complete_session,
            get_sessions,
            get_rounds,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_handle() -> audio::capture::RecordingHandle {
        audio::capture::RecordingHandle {
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Controlled path: after the worker terminates, the command explicitly
    /// clears RecordingState and disarms the guard, so a second round can
    /// acquire the recording slot immediately — no spawned-task timing.
    #[tokio::test]
    async fn recording_slot_reusable_after_explicit_clear_and_disarm() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let handle = fake_handle();

        acquire_recording(&state, handle.clone()).await.unwrap();
        let guard = RecordingGuard {
            state: state.clone(),
            armed: std::sync::atomic::AtomicBool::new(true),
        };

        // Same ordering as the controlled completion/timeout paths in
        // run_interview_round: worker done -> explicit clear -> disarm.
        clear_active_recording(&state).await;
        guard.disarm();
        drop(guard);

        assert!(state.handle.lock().await.is_none());
        // Second round can acquire the slot without waiting.
        acquire_recording(&state, handle).await.unwrap();
        assert!(state.handle.lock().await.is_some());
    }

    /// Safety net: a guard dropped while still armed (panic/cancellation path)
    /// must still clear the slot.
    #[tokio::test]
    async fn recording_slot_cleared_by_guard_safety_net_on_drop() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let handle = fake_handle();
        acquire_recording(&state, handle.clone()).await.unwrap();

        {
            let guard = RecordingGuard {
                state: state.clone(),
                armed: std::sync::atomic::AtomicBool::new(true),
            };
            drop(guard);
        }
        // Let the spawned safety-net clear task run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(state.handle.lock().await.is_none());
    }

    /// Acquire rejects while a slot is held, mirroring the duplicate-round
    /// guard in run_interview_round.
    #[tokio::test]
    async fn acquire_rejects_while_slot_held() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let handle = fake_handle();
        acquire_recording(&state, handle.clone()).await.unwrap();

        let result = acquire_recording(&state, fake_handle()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already active"));
    }
}
