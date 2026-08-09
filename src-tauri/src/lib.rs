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

/// Owner of the recording-slot lease, the worker's stop flag, AND the worker's
/// JoinHandle (P2-1). Every controlled path clears RecordingState explicitly
/// (after the worker has fully terminated) and disarms the guard, so the slot
/// is freed deterministically. If the guard is dropped while still armed — an
/// unexpected drop or panic of the outer command — Drop sets the stop flag,
/// aborts and awaits the worker, and ONLY THEN clears the slot. A bare
/// JoinHandle drop would detach the task and let the slot be cleared while
/// the worker is still alive; this guard closes that lifecycle gap.
struct RecordingGuard<T: Send + 'static> {
    state: Arc<RecordingState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<tokio::task::JoinHandle<T>>,
    armed: std::sync::atomic::AtomicBool,
}

impl<T: Send + 'static> RecordingGuard<T> {
    fn new(state: Arc<RecordingState>, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            state,
            stop,
            worker: None,
            armed: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Give the guard ownership of the spawned worker so a dropped command
    /// can abort/await it before releasing the slot.
    fn attach_worker(&mut self, handle: tokio::task::JoinHandle<T>) {
        self.worker = Some(handle);
    }

    /// Resolves when the owned worker has finished (if any). Polled via
    /// select!/timeout — it never consumes the JoinHandle, so cancellation can
    /// still abort the worker afterwards.
    async fn wait_worker(&self) {
        loop {
            match &self.worker {
                Some(handle) if !handle.is_finished() => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                _ => return,
            }
        }
    }

    /// Capture the worker's result exactly once (normal completion path).
    async fn take_result(&mut self) -> Option<Result<T, tokio::task::JoinError>> {
        match self.worker.take() {
            Some(handle) => Some(handle.await),
            None => None,
        }
    }

    /// Request abort of the owned worker (timeout path after the grace period
    /// expires).
    fn abort_worker(&self) {
        if let Some(handle) = &self.worker {
            handle.abort();
        }
    }

    /// Await the (aborted) worker so its lifecycle is fully resolved.
    async fn await_worker(&mut self) {
        if let Some(handle) = self.worker.take() {
            let _ = handle.await;
        }
    }

    /// Disarm the safety net after explicit cleanup has run, so Drop does not
    /// schedule a redundant clear/abort.
    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

impl<T: Send + 'static> Drop for RecordingGuard<T> {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            self.stop.store(true, Ordering::SeqCst);
            let worker = self.worker.take();
            let state = self.state.clone();
            // Spawn on the runtime; this runs even on panic. The slot is
            // cleared ONLY after the worker has terminated (abort + await),
            // so the slot can never be freed while a detached worker is alive.
            tokio::runtime::Handle::current().spawn(async move {
                if let Some(handle) = worker {
                    handle.abort();
                    let _ = handle.await;
                }
                clear_active_recording(&state).await;
            });
        }
    }
}

/// Maximum duration a raw `start_recording` command may run. The UI no longer
/// uses this command (the Home audio test is bounded + temp via
/// `run_audio_test`), but as a public command it must still be hard-bounded:
/// it auto-stops after this budget even if nothing ever calls stop_recording.
const START_RECORDING_MAX_SECS: u64 = 60;

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

    // P2-1: the guard owns the slot lease, stop flag, and worker JoinHandle.
    // If this command is dropped mid-flight, the worker is stopped/aborted
    // and awaited BEFORE the slot is released — never detached and leaked.
    let mut recording_guard = RecordingGuard::new(state.inner().clone(), stop_flag.clone());

    let path_clone = path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();

    // Spawn worker; drop original tx so channel closes when worker finishes
    let worker = tokio::spawn(async move {
        audio::capture::record_to_wav(path_clone, sr, 1, event_tx, stop_clone).await
    });
    recording_guard.attach_worker(worker);
    drop(tx);

    // Hard wall-clock bound (P1-2): auto-stop after START_RECORDING_MAX_SECS
    // even if the UI never calls stop_recording — the command cannot run
    // forever with a backend microphone handle open.
    let max_stop = stop_flag.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(START_RECORDING_MAX_SECS)).await;
        max_stop.store(true, Ordering::SeqCst);
    });

    let join_result = recording_guard.take_result().await;
    timer.abort();

    // Release the slot and disarm the guard on EVERY outcome before
    // propagating any error.
    clear_active_recording(&state).await;
    recording_guard.disarm();

    // Check for join failure (panic/cancellation)
    let record_result = match join_result {
        Some(Ok(result)) => result.map_err(|e| e.to_string()),
        Some(Err(e)) => Err(format!("Recording worker failed: {}", e)),
        None => Err("Recording worker handle lost".to_string()),
    }?;

    Ok(RecordingResult {
        path: record_result.file_path.to_string_lossy().to_string(),
        duration_ms: record_result.duration_ms,
    })
}

/// Result of a bounded microphone test
#[derive(serde::Serialize)]
struct AudioTestResult {
    duration_ms: u64,
    file_size_bytes: u64,
}

/// Bounded, self-cleaning microphone test (P1-2). Records to the TEMP
/// directory (never the persistent recordings/ tree), auto-stops after a hard
/// 10s wall-clock bound, and DELETES the WAV before returning. The Home page
/// Audio Test uses this instead of `start_recording`, so a test can never
/// create orphan persistent recordings and can never leave a backend
/// microphone operation running unbounded.
#[tauri::command]
async fn run_audio_test(
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<AudioTestResult, String> {
    let (tx, _rx) = mpsc::channel(32);

    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };
    acquire_recording(&state, handle).await?;

    // Same exclusive-slot ownership as interview recording and device checks:
    // an audio test can never overlap another capture.
    let mut recording_guard = RecordingGuard::new(state.inner().clone(), stop_flag.clone());

    let tmp_path = paths
        .paths
        .temp_dir
        .join(format!("audio_test_{}.wav", uuid::Uuid::new_v4()));

    let path_clone = tmp_path.clone();
    let event_tx = tx.clone();
    let stop_clone = stop_flag.clone();
    let worker = tokio::spawn(async move {
        audio::capture::record_to_wav(path_clone, 16000, 1, event_tx, stop_clone).await
    });
    recording_guard.attach_worker(worker);

    // Hard wall-clock bound: the test auto-stops after AUDIO_TEST_MAX_SECS
    // even if the UI never calls stop_recording.
    const AUDIO_TEST_MAX_SECS: u64 = 10;
    let max_stop = stop_flag.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(AUDIO_TEST_MAX_SECS)).await;
        max_stop.store(true, Ordering::SeqCst);
    });

    let join_result = recording_guard.take_result().await;
    timer.abort();

    // The test never persists evidence: delete the temp WAV (and any partial
    // temp file) regardless of outcome.
    let _ = std::fs::remove_file(&tmp_path);
    let _ = std::fs::remove_file(tmp_path.with_extension("wav.tmp"));

    // Release the slot and disarm the guard on EVERY outcome.
    clear_active_recording(&state).await;
    recording_guard.disarm();

    let record_result = match join_result {
        Some(Ok(result)) => result.map_err(|e| e.to_string()),
        Some(Err(e)) => Err(format!("Audio test worker failed: {}", e)),
        None => Err("Audio test worker handle lost".to_string()),
    }?;

    Ok(AudioTestResult {
        duration_ms: record_result.duration_ms,
        file_size_bytes: record_result.file_size_bytes,
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
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<interview::device_check::DeviceCheckResult, String> {
    let (tx, _rx) = mpsc::channel(32);

    // P2-1: route the device-test microphone capture through the SAME
    // exclusive capture-ownership mechanism as interview recording. A device
    // check cannot overlap an interview round (or another device check); the
    // slot is released deterministically on success, error, and timeout.
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };
    acquire_recording(&state, handle).await?;

    // Safety net for panic/cancellation: owns the slot lease and clears it on
    // drop if not explicitly cleared below. No worker is attached here — the
    // device test capture is internally bounded (wall-clock deadline).
    let recording_guard = RecordingGuard::<()>::new(state.inner().clone(), stop_flag);

    let result = interview::device_check::run_device_check(paths.paths.temp_dir.clone(), tx).await;

    // Release the slot on every outcome (success, error, timeout) and disarm
    // the safety net.
    clear_active_recording(&state).await;
    recording_guard.disarm();

    Ok(result)
}

/// Result of a single interview round
#[derive(serde::Serialize)]
struct InterviewRoundResult {
    metadata: interview::orchestrator::AudioMetadata,
    transcription: String,
}

/// Number of questions in the current fixed flow. Finality is derived on the
/// backend from this constant — the frontend never supplies `is_final`.
const EXPECTED_ROUNDS: i32 = 5;

/// Backend-authoritative preflight for a round request. Verifies — BEFORE any
/// audio/hardware work — that the session exists and is not completed, that
/// the requested `round_index` is non-negative and equals the backend's next
/// expected logical round (rejecting duplicates and skipped/out-of-order
/// rounds), and derives finality from `EXPECTED_ROUNDS`. Returns `Ok(true)`
/// when the round is the final one. Errors are user-facing strings.
fn preflight_round(
    db: &db::Database,
    session_id_str: &str,
    round_index: i32,
) -> Result<bool, String> {
    let session = db
        .get_session(session_id_str)
        .map_err(|e| format!("Failed to load session: {}", e))?
        .ok_or_else(|| format!("Session {} does not exist", session_id_str))?;

    if session.completed_at.is_some() {
        return Err("Session is already completed — no more rounds can run".to_string());
    }
    if round_index < 0 {
        return Err(format!("Invalid round index {}", round_index));
    }

    // P2-4: validate that persisted round history is contiguous from zero
    // BEFORE deriving the next expected round. Count-based logic alone could
    // accept a duplicate/out-of-order round after malformed stored indexes
    // (e.g. [0, 2] would make len()==2 look like the next expected round is
    // 2, silently skipping index 1).
    let existing = db
        .get_rounds(session_id_str)
        .map_err(|e| format!("Failed to load session rounds: {}", e))?;
    for (expected, round) in existing.iter().enumerate() {
        if round.round_index != expected as i32 {
            return Err("Session round history is inconsistent".to_string());
        }
    }
    let next_expected = existing.len() as i32;

    // A request beyond the fixed question count can never be valid.
    if round_index >= EXPECTED_ROUNDS {
        return Err(format!(
            "Round {} is beyond the expected {} rounds",
            round_index + 1,
            EXPECTED_ROUNDS
        ));
    }
    if round_index < next_expected {
        return Err(format!(
            "Round {} already exists for this session",
            round_index + 1
        ));
    }
    if round_index > next_expected {
        return Err(format!(
            "Round {} is out of order — expected round {}",
            round_index + 1,
            next_expected + 1
        ));
    }
    Ok(round_index == EXPECTED_ROUNDS - 1)
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
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
    db_state: State<'_, Arc<DbState>>,
    app: tauri::AppHandle,
) -> Result<InterviewRoundResult, String> {
    // Backend-authoritative session lifecycle and round order — ALL checks run
    // BEFORE any audio/hardware work. Finality is derived here, never trusted
    // from the client.
    let session_id_str = session_id.hyphenated().to_string();
    let is_final = {
        let guard = db_state.db.lock().await;
        let db = guard.as_ref().ok_or("Database not initialized")?;
        preflight_round(db, &session_id_str, round_index)?
    };

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
    // Owner of the slot lease + stop flag + worker JoinHandle (P2-1). Every
    // controlled path below clears RecordingState explicitly (after the worker
    // has fully terminated) and disarms it. If this command is dropped or
    // panics while the guard is armed, Drop sets the stop flag, aborts and
    // awaits the worker, and only then clears the slot — the slot can never
    // be freed while a detached worker is still alive.
    let mut recording_guard = RecordingGuard::new(state.inner().clone(), stop_flag.clone());

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
    recording_guard.attach_worker(worker);

    // Full-round deadline + cooperative shutdown
    const ROUND_TIMEOUT_SECS: u64 = 300;
    const GRACE_SECS: u64 = 5;

    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(ROUND_TIMEOUT_SECS);

    // Wait for the worker (polling its JoinHandle without consuming it) or the
    // deadline. The JoinHandle result is captured exactly once afterwards via
    // take_result() — the same handle is still owned by the guard for the
    // timeout/abort path.
    let timed_out = tokio::select! {
        _ = recording_guard.wait_worker() => false,
        _ = tokio::time::sleep_until(deadline) => true,
    };

    if !timed_out {
        // Normal completion path — capture the JoinHandle result exactly once.
        // The worker's phase_tx sender is dropped when the worker finishes,
        // so awaiting the relay lets every queued event flush before any
        // final event is emitted by the persistence-owning layer. Never
        // abort it here: a queued "complete"/final event must not be lost.
        let _ = phase_relay.await;

        let join_result = recording_guard.take_result().await;
        let result = match join_result {
            Some(Ok(inner)) => inner.map_err(|e| e.to_string()),
            Some(Err(join_err)) => Err(format!("Interview worker failed: {}", join_err)),
            None => Err("Interview worker handle lost".to_string()),
        };

        // RecordingState is held until persistence FULLY finishes (P1-1):
        // a concurrent request for the same logical round cannot acquire
        // the slot and start TTS/audio work while this request is between
        // worker completion and DB COMMIT.
        let round_result = match result {
            Ok((metadata, transcription, mut evidence)) => {
                // Persist round atomically: round INSERT + session
                // total_rounds increment + completed_at (when final) commit
                // in ONE transaction. The WAV stays provisional (evidence
                // armed) until this COMMIT succeeds.
                let persist_result: Result<i64, String> = (async {
                    let guard = db_state.db.lock().await;
                    let db = guard
                        .as_ref()
                        .ok_or_else(|| "Database not initialized".to_string())?;
                    db.insert_round_with_session_update(
                        &session_id_str,
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
                    Ok(_) => {
                        // Durable: disarm the evidence guard (WAV retained)
                        // and only NOW emit "complete" — it means the round
                        // is durably persisted.
                        evidence.commit();
                        let _ = app.emit(
                            "interview-phase",
                            PhaseEventPayload {
                                phase: "complete".to_string(),
                                question: None,
                                duration_ms: None,
                            },
                        );
                        Ok(InterviewRoundResult {
                            metadata,
                            transcription,
                        })
                    }
                    Err(e) => {
                        // Persistence failed — evidence is still armed and
                        // drops at the end of this arm, deleting the WAV
                        // and any partial temp file. "complete" is NEVER
                        // emitted for an unpersisted round.
                        let _ = app.emit(
                            "interview-phase",
                            PhaseEventPayload {
                                phase: "error".to_string(),
                                question: Some(e.clone()),
                                duration_ms: None,
                            },
                        );
                        Err(e)
                    }
                }
            }
            Err(e) => {
                // P2-3: every round termination must produce ONE terminal
                // backend event. The worker failed (TTS, recording,
                // checksum, transcription) — emit error before returning.
                // The relay was drained above, so this is the last word.
                let _ = app.emit(
                    "interview-phase",
                    PhaseEventPayload {
                        phase: "error".to_string(),
                        question: Some(e.clone()),
                        duration_ms: None,
                    },
                );
                Err(e)
            }
        };

        // Persistence has fully finished (COMMIT or failure) — only NOW
        // release the recording slot deterministically and disarm the
        // safety net. Every controlled path (success or error) clears the
        // slot exactly once.
        clear_active_recording(&state).await;
        recording_guard.disarm();

        round_result
    } else {
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

        // Await the SAME worker up to the 5s grace period — allows
        // cooperative exit.
        let graceful = tokio::select! {
            _ = recording_guard.wait_worker() => true,
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(GRACE_SECS)) => false,
        };

        if !graceful {
            // Grace period expired — force abort, then await the aborted
            // JoinHandle so worker lifecycle is fully resolved before this
            // command returns.
            recording_guard.abort_worker();
            recording_guard.await_worker().await;
        }

        // Worker has fully terminated (cooperatively or aborted) — release
        // the recording slot deterministically and disarm the safety net.
        clear_active_recording(&state).await;
        recording_guard.disarm();

        // Drain the relay so queued phase events flush BEFORE the final
        // error event below — the timeout error must be the last word.
        let _ = phase_relay.await;
        let _ = app.emit(
            "interview-phase",
            PhaseEventPayload {
                phase: "error".to_string(),
                question: Some(format!("Round timed out after {}s", ROUND_TIMEOUT_SECS)),
                duration_ms: None,
            },
        );

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

        Err(format!(
            "Round timed out after {}s — processes terminated, partial artifacts cleaned up",
            ROUND_TIMEOUT_SECS
        ))
    }
}

/// Retry a failed interview round with a fresh round_id. Only a round that
/// never committed may be retried: the backend preflight requires the
/// round_index to be the next expected logical round, so an already-persisted
/// round returns the explicit duplicate error and a retry cannot double-write.
/// Finality is derived on the backend from round_index.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn retry_interview_round(
    question: String,
    session_id: uuid::Uuid,
    round_index: i32,
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

// NOTE: there is deliberately NO `complete_session` Tauri command. Session
// lifecycle is backend-authoritative: the final round commits completed_at in
// the same transaction as the round insert. JavaScript cannot arbitrarily set
// total_rounds or completion state.

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
            run_audio_test,
            play_round_audio,
            generate_tts,
            list_audio_devices,
            check_audio_devices,
            run_interview_round,
            retry_interview_round,
            stop_interview_round,
            get_tools_dir,
            create_session,
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

    fn new_guard(
        state: &Arc<RecordingState>,
    ) -> (RecordingGuard<()>, Arc<std::sync::atomic::AtomicBool>) {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        (RecordingGuard::<()>::new(state.clone(), stop.clone()), stop)
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
        let (guard, _stop) = new_guard(&state);

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
            let (guard, _stop) = new_guard(&state);
            drop(guard);
        }
        // Let the spawned safety-net clear task run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(state.handle.lock().await.is_none());
    }

    /// P2-1: a guard dropped while armed with a live worker sets the stop
    /// flag, terminates the worker, and clears the slot ONLY after
    /// termination — the slot can never be freed while a detached worker is
    /// still alive (a bare JoinHandle drop would detach the task).
    #[tokio::test]
    async fn recording_guard_drop_with_live_worker_terminates_before_clearing() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            let worker_stop = stop.clone();
            let worker = tokio::spawn(async move {
                // A worker that runs until the stop flag is set.
                while !worker_stop.load(Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                "done"
            });
            guard.attach_worker(worker);
            // Dropped while still armed — simulates panic/drop of the outer
            // command before the worker completed.
        }

        // Give the drop-spawned cleanup task time to run.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert!(
            stop.load(Ordering::SeqCst),
            "drop must set the stop flag so the worker terminates"
        );
        assert!(
            state.handle.lock().await.is_none(),
            "slot must be cleared only after the worker terminated"
        );
    }

    /// P2-1: take_result captures the worker's JoinHandle result exactly once.
    #[tokio::test]
    async fn recording_guard_take_result_captures_once() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();
        let mut guard = RecordingGuard::new(state.clone(), stop.clone());

        let worker = tokio::spawn(async { 42 });
        guard.attach_worker(worker);
        guard.wait_worker().await;

        let first = guard.take_result().await;
        assert!(matches!(first, Some(Ok(42))));
        // The result was captured exactly once — a second take is empty.
        assert!(guard.take_result().await.is_none());

        clear_active_recording(&state).await;
        guard.disarm();
    }

    /// P2-1: abort_worker + await_worker (the timeout path's force-abort
    /// sequence) fully resolves the worker lifecycle and consumes the handle.
    #[tokio::test]
    async fn recording_guard_abort_then_await_resolves_worker() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();
        let mut guard = RecordingGuard::new(state.clone(), stop.clone());

        let worker_stop = stop.clone();
        let worker = tokio::spawn(async move {
            while !worker_stop.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            "done"
        });
        guard.attach_worker(worker);

        guard.abort_worker();
        guard.await_worker().await;
        assert!(
            guard.take_result().await.is_none(),
            "aborted handle is consumed by await_worker"
        );

        clear_active_recording(&state).await;
        guard.disarm();
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

    /// P1-1: the recording slot stays held across the persistence window. A
    /// concurrent request for the same logical round (Request B) cannot
    /// acquire the slot and start TTS/audio work while Request A is between
    /// worker completion and DB COMMIT. The slot is released exactly once,
    /// only after persistence finishes.
    #[tokio::test]
    async fn recording_slot_held_through_persistence_window() {
        let state = Arc::new(RecordingState {
            handle: Mutex::new(None),
        });
        let handle = fake_handle();
        acquire_recording(&state, handle.clone()).await.unwrap();
        let (guard, _stop) = new_guard(&state);

        // Request A's worker has completed but its DB transaction has NOT
        // committed yet (persistence window open — slot still held).
        let state_b = state.clone();
        let request_b =
            tokio::spawn(async move { acquire_recording(&state_b, fake_handle()).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let result_b = request_b.await.unwrap();
        assert!(
            result_b.is_err(),
            "Request B must not acquire the slot during A's persistence window"
        );
        assert!(result_b.unwrap_err().contains("already active"));

        // Persistence COMMITs -> slot cleared exactly once -> B can retry.
        clear_active_recording(&state).await;
        guard.disarm();
        drop(guard);

        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.handle.lock().await.is_some());
    }

    // ------------------------------------------------------------------
    // P1-2: Backend-authoritative session lifecycle and round order
    // ------------------------------------------------------------------

    fn temp_db() -> (tempfile::TempDir, db::Database) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("preflight.db");
        let db = db::Database::open(&db_path).unwrap();
        (dir, db)
    }

    fn insert_round(db: &db::Database, session: &str, index: i32, is_final: bool) {
        db.insert_round_with_session_update(
            session,
            index,
            &format!("Q{}", index),
            &format!("A{}", index),
            &format!("/tmp/{}.wav", index),
            "hash",
            5000,
            16000,
            1,
            160044,
            is_final,
        )
        .unwrap();
    }

    #[test]
    fn preflight_rejects_missing_session_before_audio() {
        let (_dir, db) = temp_db();
        let err = preflight_round(&db, "no-such-session", 0).unwrap_err();
        assert!(
            err.contains("does not exist"),
            "expected missing-session error, got: {}",
            err
        );
    }

    #[test]
    fn preflight_rejects_completed_session() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        // Final round completes the session.
        insert_round(&db, "s1", 4, true);
        let err = preflight_round(&db, "s1", 5).unwrap_err();
        assert!(
            err.contains("already completed"),
            "expected completed-session error, got: {}",
            err
        );
    }

    #[test]
    fn preflight_rejects_negative_round() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        let err = preflight_round(&db, "s1", -1).unwrap_err();
        assert!(
            err.contains("Invalid round index"),
            "expected negative-round error, got: {}",
            err
        );
    }

    #[test]
    fn preflight_rejects_skipped_out_of_order_round() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        insert_round(&db, "s1", 0, false);
        // Skipping round 2 (index 1) and requesting index 2 must be rejected
        // before any audio work.
        let err = preflight_round(&db, "s1", 2).unwrap_err();
        assert!(
            err.contains("out of order"),
            "expected out-of-order error, got: {}",
            err
        );
    }

    #[test]
    fn preflight_rejects_duplicate_round_before_audio() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        insert_round(&db, "s1", 0, false);
        insert_round(&db, "s1", 1, false);
        // Re-requesting an already-persisted round must fail up front.
        let err = preflight_round(&db, "s1", 0).unwrap_err();
        assert!(
            err.contains("already exists"),
            "expected duplicate error, got: {}",
            err
        );
    }

    #[test]
    fn preflight_accepts_next_expected_after_contiguous_history() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        insert_round(&db, "s1", 0, false);
        insert_round(&db, "s1", 1, false);
        // [0, 1] -> the next expected logical round is 2.
        let is_final = preflight_round(&db, "s1", 2).unwrap();
        assert!(!is_final);
    }

    #[test]
    fn preflight_rejects_gapped_stored_history() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        insert_round(&db, "s1", 0, false);
        insert_round(&db, "s1", 2, false); // index 1 missing — malformed history
        let err = preflight_round(&db, "s1", 3).unwrap_err();
        assert!(
            err.contains("inconsistent"),
            "gapped history must be rejected before audio, got: {}",
            err
        );
    }

    #[test]
    fn preflight_rejects_round_beyond_expected_count() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        // Rounds 0..EXPECTED_ROUNDS-2 committed; next expected is the final
        // round (EXPECTED_ROUNDS-1). Requesting EXPECTED_ROUNDS (one past the
        // last) must be rejected.
        for index in 0..EXPECTED_ROUNDS - 1 {
            insert_round(&db, "s1", index, false);
        }
        let err = preflight_round(&db, "s1", EXPECTED_ROUNDS).unwrap_err();
        assert!(
            err.contains("beyond the expected"),
            "round beyond EXPECTED_ROUNDS must be rejected, got: {}",
            err
        );
    }

    #[test]
    fn preflight_derives_finality_on_backend() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();
        // Rounds 0..3 are never final; round 4 (EXPECTED_ROUNDS-1) is final.
        // Each round is inserted after its preflight so the next one is the
        // expected next logical round.
        for index in 0..EXPECTED_ROUNDS {
            let is_final = preflight_round(&db, "s1", index).unwrap();
            assert_eq!(
                is_final,
                index == EXPECTED_ROUNDS - 1,
                "round {} finality must be backend-derived",
                index
            );
            insert_round(&db, "s1", index, is_final);
        }
    }

    #[test]
    fn rounds_1_to_5_update_lifecycle_correctly() {
        let (_dir, db) = temp_db();
        db.create_session("s1", "Test").unwrap();

        // Drive the full fixed five-question flow through the backend.
        for index in 0..EXPECTED_ROUNDS {
            let is_final = preflight_round(&db, "s1", index).unwrap();
            insert_round(&db, "s1", index, is_final);
        }

        let session = db.get_session("s1").unwrap().unwrap();
        assert_eq!(session.total_rounds, 5);
        assert!(
            session.completed_at.is_some(),
            "session must be completed after the final round commits"
        );
        assert_eq!(db.get_rounds("s1").unwrap().len(), 5);

        // After completion, no further rounds may run.
        let err = preflight_round(&db, "s1", 5).unwrap_err();
        assert!(err.contains("already completed"));
    }
}
