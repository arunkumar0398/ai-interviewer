pub mod audio;
pub mod cleanup;
pub mod db;
pub mod interview;
pub mod paths;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{Emitter, Manager, State};
use tokio::sync::{mpsc, Mutex};

use paths::{get_app_config, resolve_app_paths, uuid_to_path, PathsState};

/// Shared EXCLUSIVE audio/process slot (RC-G1B): ONE mutex-protected state
/// machine so that poison checking + acquisition is a single atomic
/// transition under one synchronization boundary — there is no check-then-act
/// window for a concurrent ReapFailed cleanup to slip a new operation past a
/// poison that was already set.
///
/// - `Free -> Active(handle)` admits exactly one operation.
/// - `Active -> Free` is the ordinary release.
/// - `Free | Active -> Poisoned` is the fail-closed transition; `Poisoned`
///   is sticky and rejects EVERY future acquisition with a restart-required
///   error for the remainder of the app session.
/// - `Poisoned -> Free` NEVER happens.
enum AudioSlotState {
    Free,
    Active(audio::capture::RecordingHandle),
    Poisoned {
        /// Retained stop handle of the operation that was active when the
        /// subsystem was poisoned — poisoning never loses lifecycle
        /// ownership. The ordinary release path clears only this component;
        /// the poison itself is never erased.
        active: Option<audio::capture::RecordingHandle>,
        reason: String,
    },
}

/// RC-G1: acquire must check the poison FIRST — Poisoned is distinct from
/// "already active" and is never presented as ordinary busy.
const POISONED_ACQUIRE_ERROR: &str =
    "Audio subsystem is unavailable because a previous native process could not be \
     conclusively terminated. Restart the application before continuing.";

struct RecordingState {
    slot: Mutex<AudioSlotState>,
}

impl RecordingState {
    #[cfg(test)]
    /// Whether an operation is currently admitted (Active).
    async fn is_active(&self) -> bool {
        matches!(&*self.slot.lock().await, AudioSlotState::Active(_))
    }

    #[cfg(test)]
    /// Whether the slot is in the plain reusable Free state.
    async fn slot_is_free(&self) -> bool {
        matches!(&*self.slot.lock().await, AudioSlotState::Free)
    }

    #[cfg(test)]
    /// Whether NO operation is admitted — Free, or Poisoned after its active
    /// component was released by the ordinary clear path.
    async fn slot_is_released(&self) -> bool {
        !matches!(
            &*self.slot.lock().await,
            AudioSlotState::Active(_)
                | AudioSlotState::Poisoned {
                    active: Some(_),
                    ..
                }
        )
    }

    #[cfg(test)]
    /// The sticky poison reason, if the subsystem is poisoned.
    async fn poison_reason(&self) -> Option<String> {
        match &*self.slot.lock().await {
            AudioSlotState::Poisoned { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }
}

/// Atomically acquire the recording slot: ONE lock, ONE transition. Rejects
/// with a restart-required error when poisoned (RC-G1), or as ordinary busy
/// when already active.
async fn acquire_recording(
    state: &RecordingState,
    handle: audio::capture::RecordingHandle,
) -> Result<(), String> {
    let mut slot = state.slot.lock().await;
    match &mut *slot {
        AudioSlotState::Free => {
            *slot = AudioSlotState::Active(handle);
            Ok(())
        }
        AudioSlotState::Active(_) => Err("A recording is already active".to_string()),
        AudioSlotState::Poisoned { reason, .. } => {
            Err(format!("{POISONED_ACQUIRE_ERROR} (reason: {reason})"))
        }
    }
}

/// Ordinary release: `Active -> Free`, `Free -> Free`. On a Poisoned state
/// only the retained active component is cleared — the poison is sticky and
/// NEVER erased (RC-G1).
async fn clear_active_recording(state: &RecordingState) {
    let mut slot = state.slot.lock().await;
    match &mut *slot {
        AudioSlotState::Active(_) => *slot = AudioSlotState::Free,
        AudioSlotState::Poisoned { active, .. } => *active = None,
        AudioSlotState::Free => {}
    }
}

/// RC-G1: mark the shared audio/process subsystem poisoned/unavailable under
/// the SAME lock as acquisition. The FIRST recorded reason is kept and later
/// failures never downgrade the state to Free/reusable. An active handle is
/// retained inside the Poisoned state so lifecycle ownership is never lost.
async fn poison_recording(state: &RecordingState, reason: &str) {
    let mut slot = state.slot.lock().await;
    let current = std::mem::replace(&mut *slot, AudioSlotState::Free);
    *slot = match current {
        AudioSlotState::Free => AudioSlotState::Poisoned {
            active: None,
            reason: reason.to_string(),
        },
        AudioSlotState::Active(handle) => AudioSlotState::Poisoned {
            active: Some(handle),
            reason: reason.to_string(),
        },
        poisoned @ AudioSlotState::Poisoned { .. } => poisoned,
    };
}

/// Holds the database connection
struct DbState {
    db: Mutex<Option<db::Database>>,
}

/// Owner of the recording-slot lease, the worker's stop flag, the worker's
/// JoinHandle (P2-1), AND the physical worker lifecycle signals (P1-1 +
/// RC-2): one for the microphone capture (input probe) and one for the
/// speaker probe (output probe). Every controlled path clears
/// RecordingState explicitly (after the worker has fully terminated) and
/// disarms the guard, so the slot is freed deterministically. If the guard
/// is dropped while still armed — an unexpected drop or panic of the outer
/// command — Drop sets the stop flag, waits for EVERY scheduled REAL
/// blocking worker to terminate (bounded cooperative grace, then
/// force-abort of the outer wrapper, then waiting out the blocking
/// workers), and ONLY THEN clears the slot. A bare JoinHandle drop would
/// detach the task and let the slot be cleared while a worker is still
/// alive; aborting the outer wrapper alone does not prove the nested
/// `spawn_blocking` closures have stopped — the slot is never freed while
/// any physical input/output worker owned by the operation may still be
/// alive.
struct RecordingGuard<T: Send + 'static> {
    state: Arc<RecordingState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Physical MICROPHONE-capture lifecycle signal (input probe).
    completion: audio::capture::CaptureCompletion,
    /// Physical SPEAKER-probe lifecycle signal (output probe, RC-2). The
    /// device check also starts a real output stream; if the owning command
    /// is cancelled while that probe is inside spawn_blocking, the blocking
    /// output task can outlive the outer worker — so the slot must stay
    /// occupied until BOTH completions are resolved.
    output_completion: audio::capture::CaptureCompletion,
    /// Native child-process lifecycle signal (RC-2B): Piper/Whisper children
    /// are marked Running before spawn and Reaped only after kill + wait, so
    /// a force-abort can never resolve slot ownership while a child may be
    /// alive.
    process_completion: audio::pipe::ProcessCompletion,
    worker: Option<tokio::task::JoinHandle<T>>,
    armed: std::sync::atomic::AtomicBool,
    /// Drop-safety-net cooperative grace. Tests shrink this to exercise the
    /// force-abort path quickly.
    drop_grace: std::time::Duration,
}

impl<T: Send + 'static> RecordingGuard<T> {
    fn new(state: Arc<RecordingState>, stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            state,
            stop,
            completion: audio::capture::CaptureCompletion::new(),
            output_completion: audio::capture::CaptureCompletion::new(),
            process_completion: audio::pipe::ProcessCompletion::default(),
            worker: None,
            armed: std::sync::atomic::AtomicBool::new(true),
            drop_grace: std::time::Duration::from_secs(RECORDING_DROP_GRACE_SECS),
        }
    }

    /// Clone of the physical-capture lifecycle signal, handed to the worker
    /// so `record_to_wav` / `record_test_clip` can report termination.
    fn completion(&self) -> audio::capture::CaptureCompletion {
        self.completion.clone()
    }

    /// Clone of the SPEAKER-probe lifecycle signal (RC-2), handed to the
    /// device-check worker so `validate_production_playback_stream` can
    /// report termination of its real output stream.
    fn output_completion(&self) -> audio::capture::CaptureCompletion {
        self.output_completion.clone()
    }

    /// Clone of the native child-process lifecycle signal (RC-2B), handed to
    /// the interview worker so Piper/Whisper can report kill+wait completion.
    fn process_completion(&self) -> audio::pipe::ProcessCompletion {
        self.process_completion.clone()
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

/// Wait for an owned recording worker without consuming its JoinHandle until
/// it has finished. On deadline expiry the caller returns an error with the
/// guard still armed, so Drop retains the slot and all physical-worker
/// lifecycle signals until deferred cleanup has actually completed.
async fn take_recording_result_before_deadline<T: Send + 'static>(
    guard: &mut RecordingGuard<T>,
    deadline: std::time::Duration,
    timeout_error: &str,
) -> Result<Option<Result<T, tokio::task::JoinError>>, String> {
    tokio::time::timeout(deadline, guard.wait_worker())
        .await
        .map_err(|_| timeout_error.to_string())?;
    Ok(guard.take_result().await)
}

/// Bounded cooperative-shutdown grace for the RecordingGuard drop safety net
/// (P1-1). After the stop flag is set, the nested blocking capture is given
/// this long to observe the flag and terminate NORMALLY. Only when the grace
/// expires is the outer worker force-aborted. 5s comfortably exceeds the
/// ~100ms capture-loop poll interval while keeping an unexpected-drop cleanup
/// bounded.
const RECORDING_DROP_GRACE_SECS: u64 = 5;

impl<T: Send + 'static> Drop for RecordingGuard<T> {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            self.stop.store(true, Ordering::SeqCst);
            let worker = self.worker.take();
            let state = self.state.clone();
            let completion = self.completion.clone();
            let output_completion = self.output_completion.clone();
            let process_completion = self.process_completion.clone();
            let grace = self.drop_grace;
            // Spawn on the runtime; this runs even on panic. The slot is
            // cleared ONLY after the worker's lifecycle is resolved AND every
            // physical worker the operation may have started has terminated:
            // the microphone capture (input probe), the speaker probe
            // (output probe, RC-2), and any native child process (Piper /
            // Whisper, RC-2B).
            //
            // Order (RC-2A):
            //  1. stop flag set;
            //  2. bounded cooperative grace across the CURRENTLY scheduled
            //     physical workers (advisory initial snapshot);
            //  3. if the grace expires, force-abort the outer wrapper
            //     (aborting an async task cannot kill an already-submitted
            //     spawn_blocking closure) and await it so the outer
            //     scheduler is DEFINITIVELY dead; otherwise await the outer
            //     wrapper normally;
            //  4. AUTHORITATIVE POST-ABORT RESCAN: re-read EVERY physical
            //     lifecycle tracker and wait out any still Scheduled —
            //     including one that became Scheduled after the initial
            //     snapshot (e.g. the device check's output probe scheduled
            //     at the input probe's grace boundary). The pre-abort
            //     snapshot is NOT authoritative;
            //  5. wait any Running native child to be Reaped (the aborted
            //     task's own RAII guard spawns the kill+wait task);
            //  6. ONLY THEN clear the slot.
            tokio::runtime::Handle::current().spawn(async move {
                if let Some(handle) = worker.as_ref() {
                    // 2. Bounded cooperative grace across the currently
                    // scheduled physical workers.
                    let mut any_scheduled = false;
                    let mut grace_expired = false;
                    for c in [&completion, &output_completion] {
                        if c.scheduled() {
                            any_scheduled = true;
                            if tokio::time::timeout(grace, c.wait()).await.is_err() {
                                grace_expired = true;
                            }
                        }
                    }
                    if !any_scheduled || grace_expired {
                        // 3a. Nothing was scheduled (the outer wrapper is the
                        // whole lifecycle so far) or the grace expired:
                        // force-abort the outer wrapper (aborting an async
                        // task cannot kill the blocking closures), then await
                        // it so the outer scheduler is confirmed dead.
                        handle.abort();
                        if let Some(h) = worker {
                            let _ = h.await;
                        }
                    } else {
                        // 3b. All scheduled workers finished within the
                        // grace — resolve the OUTER wrapper too: the slot
                        // clears only after the outer JoinHandle is fully
                        // resolved.
                        if let Some(h) = worker {
                            let _ = h.await;
                        }
                    }
                    // 4. AUTHORITATIVE post-abort rescan (RC-2A): the outer
                    // scheduler is dead, so the lifecycle-tracker set is now
                    // static. Re-read EVERY physical worker and wait out any
                    // still Scheduled — a worker that became Scheduled after
                    // the initial snapshot must keep the slot occupied.
                    for c in [&completion, &output_completion] {
                        if c.scheduled() {
                            c.wait().await;
                        }
                    }
                    // 5. Native child processes (RC-2B): a child that was
                    // Running when the outer worker died is killed + waited
                    // by the detached task spawned from the aborted task's
                    // own RAII guard; the slot is not released until Reaped.
                    // RC-G1 fail-closed via the SHARED policy: a conclusive
                    // Reaped releases normally; a failed wait (ReapFailed)
                    // atomically POISONS the audio subsystem so no later
                    // operation can reuse the slot while a child state is
                    // unresolved — the user must restart before continuing.
                    let _ = resolve_process_completion(
                        &state,
                        &process_completion,
                        "outer-worker abort",
                    )
                    .await;
                }
                // 6. Only now is the shared audio slot released. The poison,
                // if any, is sticky and survives this clear (RC-G1).
                clear_active_recording(&state).await;
            });
        }
    }
}

/// Result of a bounded microphone test
#[derive(serde::Serialize)]
struct AudioTestResult {
    duration_ms: u64,
    file_size_bytes: u64,
}

/// Bounded, self-cleaning microphone test (P1-2) — the ONLY raw microphone
/// test path in the app (RC-5). Records to the TEMP directory (never the
/// persistent recordings/ tree), auto-stops after 10s (bounds normal
/// recording duration), and DELETES the WAV before returning, so a test can
/// never create orphan persistent recordings and can never leave a backend
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
    let completion = recording_guard.completion();
    // RC-1: the outer wrapper does NOT own cleanup — the PHYSICAL capture
    // closure does (retain_output = false). A command-level timeout aborts
    // this outer async task, but aborting an async task cannot kill an
    // already-submitted spawn_blocking capture. The only cleanup that
    // survives such an abort is the one running INSIDE the physical closure
    // (delete final WAV, then signal Finished), so a late capture
    // finalization can never leave an orphan audio_test_*.wav.
    let worker = tokio::spawn(async move {
        audio::capture::record_to_wav(
            path_clone, 16000, 1, event_tx, stop_clone, completion,
            false, // RC-1: the physical worker removes the test WAV on success
        )
        .await
    });
    recording_guard.attach_worker(worker);

    // Recording-duration bound (RC-4): the test auto-stops after
    // AUDIO_TEST_MAX_SECS. If a native device call blocks, the audio slot
    // stays occupied until the physical worker exits rather than detaching
    // it.
    const AUDIO_TEST_MAX_SECS: u64 = 10;
    const AUDIO_TEST_COMMAND_DEADLINE_SECS: u64 = 15;
    let max_stop = stop_flag.clone();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(AUDIO_TEST_MAX_SECS)).await;
        max_stop.store(true, Ordering::SeqCst);
    });

    let join_result = take_recording_result_before_deadline(
        &mut recording_guard,
        std::time::Duration::from_secs(AUDIO_TEST_COMMAND_DEADLINE_SECS),
        "Audio test timed out while waiting for the audio device",
    )
    .await?;
    timer.abort();

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

// --- Phase 2 Commands ---

#[tauri::command]
async fn check_audio_devices(
    state: State<'_, Arc<RecordingState>>,
    paths: State<'_, PathsState>,
) -> Result<interview::device_check::DeviceCheckResult, String> {
    let (tx, _rx) = mpsc::channel(32);

    // P2-2: route the device-test microphone capture through the SAME
    // exclusive capture-ownership mechanism as interview recording, AND own
    // the actual device-check worker with the same lifecycle guarantees: the
    // shared stop flag is propagated into the test-clip capture loop, so on
    // cancellation the worker terminates (cleaning its temp WAV) before the
    // slot is released. A dropped command can never leave a detached blocking
    // capture or free the slot while the capture is still alive.
    let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = audio::capture::RecordingHandle {
        stop: stop_flag.clone(),
    };
    acquire_recording(&state, handle).await?;

    let mut recording_guard = RecordingGuard::new(state.inner().clone(), stop_flag.clone());

    let temp_dir = paths.paths.temp_dir.clone();
    let worker_stop = stop_flag.clone();
    let completion = recording_guard.completion();
    let speaker_completion = recording_guard.output_completion();
    let worker = tokio::spawn(async move {
        interview::device_check::run_device_check(
            temp_dir,
            tx,
            worker_stop,
            completion,
            speaker_completion,
        )
        .await
    });
    recording_guard.attach_worker(worker);

    // Release the slot on every outcome (success, error, timeout) and disarm
    // the safety net.
    const DEVICE_CHECK_COMMAND_DEADLINE_SECS: u64 = 10;
    let join_result = take_recording_result_before_deadline(
        &mut recording_guard,
        std::time::Duration::from_secs(DEVICE_CHECK_COMMAND_DEADLINE_SECS),
        "Audio device check timed out while waiting for the audio device",
    )
    .await?;
    clear_active_recording(&state).await;
    recording_guard.disarm();

    let result = match join_result {
        Some(Ok(result)) => result,
        Some(Err(e)) => return Err(format!("Device check worker failed: {}", e)),
        None => return Err("Device check worker handle lost".to_string()),
    };

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

/// Maximum accepted question length at the Rust boundary.
const MAX_QUESTION_LEN: usize = 10_000;

/// Validate question text at the Rust boundary (P2-1). Runs BEFORE any DB
/// preflight, recording-slot acquisition, or hardware work: a blank or
/// oversized question must never reach TTS, recording, or persistence.
/// Returns the TRIMMED text, used consistently for TTS, phase reporting,
/// retry, and persistence.
fn validate_question(question: &str) -> Result<String, String> {
    let question = question.trim();
    if question.is_empty() {
        return Err("Question cannot be empty".to_string());
    }
    // P3: the limit is CHARACTERS, not UTF-8 bytes — `chars().count()` keeps
    // the error message truthful for multibyte text.
    if question.chars().count() > MAX_QUESTION_LEN {
        return Err(format!(
            "Question too long (max {} characters)",
            MAX_QUESTION_LEN
        ));
    }
    Ok(question.to_string())
}

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
    // Backend question validation (P2-1) — the FIRST check, before any DB
    // preflight, slot acquisition, or audio work. The trimmed text is used
    // consistently for TTS, phase reporting, retry, and persistence.
    let question = validate_question(&question)?;

    // Backend-authoritative session lifecycle and round order — ALL checks run
    // BEFORE any audio/hardware work. Finality is derived here, never trusted
    // from the client.
    let session_id_str = session_id.hyphenated().to_string();
    let is_final = {
        let guard = db_state.db.lock().await;
        let db = guard.as_ref().ok_or("Database not initialized")?;
        preflight_round(db, &session_id_str, round_index)?
    };

    // RC-1: reject a round whose WAV already exists at the backend round
    // boundary — BEFORE any TTS/audio work. A replayed round_id must never
    // touch a pre-existing committed WAV. This is the FIRST line of defense;
    // the evidence guard (created only after capture succeeds) and
    // record_to_wav's own rejection are the others.
    ensure_round_evidence_absent(&paths.paths, session_id, round_id, round_index)?;

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

    // Spawn worker; guaranteed cleanup on all paths. The PRODUCTION TTS
    // playback lifecycle (RC-1) is owned by the guard's output completion:
    // the worker forwards it through the orchestrator into the physical
    // Piper question-playback worker, so the shared audio slot stays
    // occupied until that worker has fully exited.
    let question_clone = question.clone();
    let completion = recording_guard.completion();
    let output_completion = recording_guard.output_completion();
    let process_completion = recording_guard.process_completion();
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
            completion,
            output_completion,
            process_completion,
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

        // RC-G1A: resolve the native child lifecycle BEFORE any transition
        // that makes the subsystem reusable. A ReapFailed outcome atomically
        // poisons the shared subsystem (sticky) and the round must NOT claim
        // successful completion.
        let lifecycle_error = resolve_process_completion(
            &state,
            &recording_guard.process_completion(),
            "round completion",
        )
        .await
        .err();

        // RecordingState is held until persistence FULLY finishes (P1-1):
        // a concurrent request for the same logical round cannot acquire
        // the slot and start TTS/audio work while this request is between
        // worker completion and DB COMMIT.
        let round_result = if let Some(lifecycle_error) = lifecycle_error {
            // RC-G1A: the native child lifecycle is unresolved (ReapFailed) —
            // the subsystem is already poisoned (sticky). Never emit
            // "complete" for an unresolved lifecycle; preserve evidence per
            // existing rules and surface the restart-required condition.
            match result {
                Ok((metadata, _, mut evidence)) => {
                    // The worker itself completed, but the round must not
                    // claim success while a child lifecycle is unresolved.
                    // Preserve the captured evidence (never destroy it — it
                    // is real captured audio) and journal it for startup
                    // reconciliation, exactly like the ambiguous-COMMIT path.
                    evidence.preserve();
                    let note = record_reconciliation_needed(
                        &paths.paths,
                        &session_id_str,
                        round_index,
                        &metadata.file_path,
                        &metadata.sha256,
                        "round completed with unresolved native process lifecycle",
                    );
                    let message = format!("{lifecycle_error} — {note}");
                    let _ = app.emit(
                        "interview-phase",
                        PhaseEventPayload {
                            phase: "error".to_string(),
                            question: Some(message.clone()),
                            duration_ms: None,
                        },
                    );
                    Err(message)
                }
                Err(e) => {
                    // Preserve the original cause and append the stronger
                    // restart-required lifecycle condition.
                    let message = format!("{e}. {lifecycle_error}");
                    let _ = app.emit(
                        "interview-phase",
                        PhaseEventPayload {
                            phase: "error".to_string(),
                            question: Some(message.clone()),
                            duration_ms: None,
                        },
                    );
                    Err(message)
                }
            }
        } else {
            match result {
                Ok((metadata, transcription, mut evidence)) => {
                    // Persist round atomically: round INSERT + session
                    // total_rounds increment + completed_at (when final) commit
                    // in ONE transaction. The WAV stays provisional (evidence
                    // armed) until this COMMIT succeeds.
                    let persist_outcome = {
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
                    };

                    match persist_outcome {
                        db::PersistenceOutcome::Committed(_) => {
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
                        db::PersistenceOutcome::NotCommitted { source } => {
                            // Persistence is CONCLUSIVELY absent — the round was
                            // never committed. Remove the provisional evidence
                            // with explicit controlled cleanup (RC-1C): the
                            // result is observable and failures are surfaced,
                            // never silently converted to success.
                            let cleanup_result = evidence.cleanup();
                            let message = match cleanup_result {
                                Ok(()) => source,
                                Err(cleanup_err) => {
                                    format!("{} — {}", source, cleanup_err)
                                }
                            };
                            // "complete" is NEVER emitted for an unpersisted
                            // round.
                            let _ = app.emit(
                                "interview-phase",
                                PhaseEventPayload {
                                    phase: "error".to_string(),
                                    question: Some(message.clone()),
                                    duration_ms: None,
                                },
                            );
                            Err(message)
                        }
                        db::PersistenceOutcome::Unknown { source } => {
                            // AMBIGUOUS commit state (RC-4): the WAV is NEVER
                            // deleted based on an unverified assumption. The
                            // evidence is preserved in place, the
                            // reconciliation-needed state is durably recorded,
                            // and an explicit persistence-uncertain error is
                            // surfaced. Startup reconciliation reconciles the
                            // evidence against the DB (matching row -> retain;
                            // unmatched -> quarantine, never delete).
                            evidence.preserve();
                            let note = record_reconciliation_needed(
                                &paths.paths,
                                &session_id_str,
                                round_index,
                                &metadata.file_path,
                                &metadata.sha256,
                                &source,
                            );
                            let message =
                                format!("Round persistence is uncertain: {}. {}", source, note);
                            let _ = app.emit(
                                "interview-phase",
                                PhaseEventPayload {
                                    phase: "error".to_string(),
                                    question: Some(message.clone()),
                                    duration_ms: None,
                                },
                            );
                            Err(message)
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
        // RC-7: NO terminal event is emitted here. The single terminal
        // timeout error is emitted exactly once below, after the worker
        // lifecycle is resolved and the phase relay is drained, so the
        // backend's one-terminal-event-per-round contract holds (a timeout
        // produces exactly one error event).
        stop_flag.store(true, Ordering::SeqCst);

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

        // RC-2/RC-3: the outer worker is resolved, but a capture that was
        // Scheduled (submitted to the blocking pool) but not Finished may
        // still be running — or may not even have STARTED yet (queued on a
        // busy blocking pool). Aborting the outer async task proves nothing
        // about the physical closure. Until it terminates, the recording
        // slot stays occupied AND the provisional artifacts must NOT be
        // removed: a late `.wav.tmp -> .wav` rename after this command
        // returned would resurrect an orphan final WAV with no DB round.
        let completion = recording_guard.completion();
        // RC-1: the same holds for the PRODUCTION TTS playback worker — it
        // runs in its own spawn_blocking inside the outer worker, so an
        // abort of the outer task cannot kill an output closure that is
        // inside native audio calls. If its lifecycle is still Scheduled,
        // the shared audio slot must stay occupied until it signals
        // Finished, regardless of whether the outer worker exited
        // cooperatively or was force-aborted (a graceful exit awaits the
        // closure, so Scheduled here means the closure is genuinely still
        // alive).
        let output_completion = recording_guard.output_completion();
        // RC-2B: a native child (Piper/Whisper) that was Running when the
        // outer worker died is being killed + reaped by the detached task
        // spawned from the aborted task's own RAII guard. Slot/evidence
        // ownership must not resolve before that reap completes.
        let process_completion = recording_guard.process_completion();
        // RC-G1 fail-closed: a conclusive Reaped is normal; a failed wait
        // (ReapFailed) poisons the audio subsystem so no later round/audio
        // operation can reuse the slot, and the terminal message below
        // surfaces the explicit restart-required lifecycle error.
        let reap_unresolved =
            resolve_process_lifecycle_at_timeout(&state, &process_completion).await;
        // A worker that exited cooperatively finished its capture; a capture
        // that Finished or was never Scheduled has nothing left to run. Only
        // Scheduled-but-not-Finished can still do physical work.
        let capture_pending = !graceful && completion.scheduled();
        let output_pending = output_completion.scheduled();

        let defer_cleanup = if capture_pending {
            // Bounded wait for the physical capture to terminate
            // cooperatively (it observes the stop flag within ~100ms).
            let terminated = tokio::time::timeout(
                std::time::Duration::from_secs(GRACE_SECS),
                completion.wait(),
            )
            .await
            .is_ok();
            !terminated
        } else {
            false
        };

        // Drain the relay so queued phase events flush BEFORE the final
        // error event below — the timeout error must be the last word
        // (RC-7: exactly one terminal event per timed-out round).
        let _ = phase_relay.await;

        // RC-7: truthful messaging — cleanup completion is reported only when
        // it actually happened (see `round_timeout_message`). RC-G1: a
        // ReapFailed lifecycle is surfaced as an explicit restart-required
        // condition appended to the terminal message, never folded into a
        // generic timeout; the controlled artifact cleanup below still runs
        // exactly as before.
        let timeout_message = if defer_cleanup || output_pending {
            round_timeout_message(defer_cleanup, output_pending, false, ROUND_TIMEOUT_SECS)
        } else {
            // No live physical capture (worker exited cooperatively, or the
            // capture terminated within the grace, or none was ever
            // Scheduled): the capture lifecycle is now static, so compute
            // ownership and remove ONLY this invocation's artifacts, then
            // release the slot — capture terminated -> owned artifacts
            // removed -> slot released (RC-3). Cleanup failures are
            // observable and reflected in the terminal message.
            let ownership =
                round_artifact_ownership(&paths.paths, session_id, round_id, &completion);
            let cleanup_failed = ownership.remove_owned() > 0;
            round_timeout_message(
                defer_cleanup,
                output_pending,
                cleanup_failed,
                ROUND_TIMEOUT_SECS,
            )
        };
        let timeout_message = if reap_unresolved {
            format!(
                "{timeout_message}. A native process could not be conclusively terminated — the audio subsystem is unavailable until the application is restarted."
            )
        } else {
            timeout_message
        };
        let _ = app.emit(
            "interview-phase",
            PhaseEventPayload {
                phase: "error".to_string(),
                question: Some(timeout_message.clone()),
                duration_ms: None,
            },
        );

        if defer_cleanup {
            // Pathological: physical capture still alive after the grace.
            // This detached task owns the TAIL lifecycle (RC-3/RC-2) in the
            // required order: real capture termination -> OWNED provisional
            // artifact cleanup -> recording slot release. The command still
            // returns bounded; a late rename can never resurrect an orphan
            // WAV, a pre-existing committed WAV is never touched, and the
            // slot is never freed while physical capture may still run.
            let cleanup_paths = paths.paths.clone();
            let cleanup_state = state.inner().clone();
            tokio::spawn(async move {
                deferred_timeout_cleanup(
                    completion,
                    &cleanup_paths,
                    session_id,
                    round_id,
                    cleanup_state,
                )
                .await;
            });
            recording_guard.disarm();
        } else if output_pending {
            // RC-1: the PRODUCTION question playback may still be physically
            // alive after the outer worker was aborted — the blocking output
            // closure is not killed by an async abort. The round never
            // reached the capture phase (playback precedes recording), so
            // there are no round artifacts to clean. Wait for the physical
            // output worker to fully exit, then release the slot: the slot
            // is never freed while production TTS playback may still be
            // running (the invariant `Scheduled -> slot MUST stay occupied`
            // holds across the outer-wrapper abort).
            let output_state = state.inner().clone();
            tokio::spawn(async move {
                output_completion.wait().await;
                clear_active_recording(&output_state).await;
            });
            recording_guard.disarm();
        } else {
            clear_active_recording(&state).await;
            recording_guard.disarm();
        }

        Err(timeout_message)
    }
}

/// RC-G1 SHARED process-finalization policy — the ONE policy used at every
/// operation-release boundary (normal worker completion, round timeout,
/// RecordingGuard Drop/cancellation cleanup).
///
/// - `NotStarted` -> `Ok` (no native process ever existed).
/// - `Reaped` -> `Ok` (conclusive wait; release normally).
/// - `ReapFailed` -> atomically poison the shared subsystem under the same
///   lock acquisition uses, and return the canonical restart-required
///   lifecycle error. Bounded: a terminal `ReapFailed` resolves immediately,
///   so this never waits forever on an unreapable process.
///
/// The poison is set BEFORE the caller performs any reusable release, so no
/// future native audio/process operation can begin after a ReapFailed.
async fn resolve_process_completion(
    state: &RecordingState,
    completion: &audio::pipe::ProcessCompletion,
    context: &str,
) -> Result<(), String> {
    if !completion.started() {
        return Ok(());
    }
    if completion.wait().await {
        Ok(())
    } else {
        poison_recording(
            state,
            &format!("{context}: a native process could not be conclusively reaped"),
        )
        .await;
        eprintln!(
            "[process-lifecycle] {context}: process reap unresolved — child state not proven Reaped; audio subsystem poisoned, restart required"
        );
        Err(format!(
            "{POISONED_ACQUIRE_ERROR} ({context}: a native process could not be conclusively terminated)"
        ))
    }
}

/// RC-G1 round-timeout wrapper: returns whether the lifecycle is unresolved
/// (subsystem poisoned) so the timeout path can append the restart-required
/// condition to its terminal message. Delegates to the shared policy.
async fn resolve_process_lifecycle_at_timeout(
    state: &RecordingState,
    process_completion: &audio::pipe::ProcessCompletion,
) -> bool {
    resolve_process_completion(state, process_completion, "round timeout")
        .await
        .is_err()
}

/// RC-7: truthful terminal timeout message, chosen from the ACTUAL cleanup
/// state. Cleanup completion is claimed only when it truly happened: a
/// deferred cleanup (physical worker still alive after the grace) explicitly
/// says the artifacts will be removed after the worker exits, a pending
/// output worker reports that playback is still shutting down, and a failed
/// cleanup reports that artifacts will be reconciled at startup — none ever
/// claims cleanup already finished when it did not.
fn round_timeout_message(
    defer_cleanup: bool,
    output_pending: bool,
    cleanup_failed: bool,
    timeout_secs: u64,
) -> String {
    if defer_cleanup {
        format!(
            "Round timed out after {}s. Cancellation was requested; temporary artifacts will be removed after the audio worker exits.",
            timeout_secs
        )
    } else if output_pending {
        format!(
            "Round timed out after {}s. Cancellation was requested; audio playback is still shutting down.",
            timeout_secs
        )
    } else if cleanup_failed {
        format!(
            "Round timed out after {}s — processes terminated, but some temporary artifacts could not be removed and will be reconciled at next startup",
            timeout_secs
        )
    } else {
        format!(
            "Round timed out after {}s — processes terminated, partial artifacts cleaned up",
            timeout_secs
        )
    }
}

/// Classification of a `reconciliation-needed.log` entry against the
/// authoritative DB state (RC-F2).
enum JournalResolution {
    /// The round row exists with matching identity — the commit went through
    /// and the entry is resolved.
    Committed,
    /// No round row exists — persistence is conclusively absent; the
    /// evidence was either quarantined by the WAV reconciliation above or is
    /// already absent. The entry is resolved.
    NotCommitted,
    /// DB/checksum state cannot be conclusively classified — keep pending.
    Unknown(String),
    /// The record could not be parsed — preserved verbatim, never dropped.
    Malformed,
}

/// Parse a journal line (`{ts} | session=.. | round_index=.. | audio_path=..
/// | sha256=.. | source=..`) into its resolution-relevant fields. Returns
/// `None` when a required field is missing/unparseable or the session is not
/// a well-formed UUID — the caller preserves the raw line (RC-F2: malformed
/// records are never silently discarded).
fn parse_journal_entry(line: &str) -> Option<(String, i32, String, String)> {
    let mut session: Option<String> = None;
    let mut round_index: Option<i32> = None;
    let mut audio_path: Option<String> = None;
    let mut sha256: Option<String> = None;
    for field in line.split('|') {
        let field = field.trim();
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "session" => session = Some(value.to_string()),
            "round_index" => round_index = value.parse::<i32>().ok(),
            "audio_path" => audio_path = Some(value.to_string()),
            "sha256" => sha256 = Some(value.to_string()),
            _ => {}
        }
    }
    let (session, round_index, audio_path, sha256) = (session?, round_index?, audio_path?, sha256?);
    if uuid::Uuid::parse_str(&session).is_err() {
        return None;
    }
    Some((session, round_index, audio_path, sha256))
}

/// Classify one journal entry against authoritative DB state (RC-F2/RC-G2).
/// The caller NEVER deletes evidence based on this classification — it only
/// decides whether the pending record may be dropped.
fn resolve_journal_entry(line: &str, db: &db::Database) -> JournalResolution {
    let Some((session, round_index, audio_path, sha256)) = parse_journal_entry(line) else {
        return JournalResolution::Malformed;
    };
    classify_journal_identity(
        db.round_identity(&session, round_index)
            .map_err(|e| e.to_string()),
        &audio_path,
        &sha256,
    )
}

/// RC-G2: strict evidence identity. A journal entry may only resolve as
/// Committed when the authoritative DB row matches BOTH the journal sha256
/// AND the journal audio_path. Any partial identity (path-only or SHA-only
/// match) stays Unknown/pending — reconciliation-needed evidence whose
/// identity disagrees in any respect remains suspicious and is never treated
/// as conclusively committed. This classification NEVER deletes evidence; it
/// only decides whether the pending record may be dropped.
fn classify_journal_identity(
    stored: Result<Option<(String, String)>, String>,
    journal_path: &str,
    journal_sha: &str,
) -> JournalResolution {
    match stored {
        Err(e) => JournalResolution::Unknown(format!("DB query failed: {e}")),
        Ok(None) => JournalResolution::NotCommitted,
        Ok(Some((stored_sha, stored_path))) => {
            if stored_sha == journal_sha && stored_path == journal_path {
                JournalResolution::Committed
            } else {
                JournalResolution::Unknown(format!(
                    "round row exists but identity is not an exact match (stored sha {stored_sha:?} vs journal {journal_sha:?}; stored path {stored_path:?} vs journal {journal_path:?}) — partial identity is not Committed"
                ))
            }
        }
    }
}

/// Split `<stem>.<ext>` — used to build collision-safe quarantine names
/// (RC-F3).
fn split_extension(file_name: &str) -> (String, String) {
    match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (file_name.to_string(), String::new()),
    }
}

/// Atomically rewrite the reconciliation journal with ONLY the still-
/// unresolved lines (RC-F2): write a `.tmp` sibling, then rename over the
/// original. On any failure the original journal is left untouched.
fn rewrite_reconciliation_journal(journal: &std::path::Path, kept: &[&str]) -> bool {
    let tmp = journal.with_extension("log.tmp");
    let mut content = String::new();
    for line in kept {
        content.push_str(line);
        content.push('\n');
    }
    if std::fs::write(&tmp, content).is_err() {
        return false;
    }
    std::fs::rename(&tmp, journal).is_ok()
}

/// RC-1E/RC-4/RC-F2/RC-F3: startup reconciliation of candidate EVIDENCE
/// (final WAVs under `recordings/`) and of the reconciliation-needed journal.
///
/// Evidence is NEVER deleted: a WAV with a matching DB round is durable
/// evidence and is left untouched; a WAV with no DB reference is ambiguous
/// orphaned evidence and is MOVED to the quarantine directory (preserved for
/// review, reported). Quarantine preserves the source session identity
/// (`quarantine/<session>/<file>`) and never overwrites an existing artifact
/// (RC-F3).
///
/// The reconciliation-needed journal is then RESOLVED against the DB (RC-F2):
/// entries whose round is now conclusively committed (or conclusively
/// absent) are dropped; unresolved/anomalous/malformed entries remain
/// pending, and the journal is rewritten atomically with only those. Journal
/// state never deletes evidence — resolution only removes the pending record.
fn reconcile_orphaned_evidence(paths: &crate::paths::AppPaths, db: &db::Database) -> Vec<String> {
    let mut messages = Vec::new();
    let quarantine = paths.quarantine_dir();

    // 1. Final WAVs under recordings/ — reconcile against the DB FIRST so the
    //    journal resolution below can classify NotCommitted entries whose
    //    evidence has now been moved to quarantine.
    if let Ok(sessions) = std::fs::read_dir(&paths.recordings_dir) {
        for session in sessions.flatten() {
            let session_dir = session.path();
            if !session_dir.is_dir() {
                continue;
            }
            let session_name = match session_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
            {
                Some(name) if crate::paths::validate_path_component(&name).is_ok() => name,
                _ => {
                    messages.push(format!(
                        "[startup-reconcile] skipping recordings entry with unsafe name: {}",
                        session_dir.display()
                    ));
                    continue;
                }
            };
            if let Ok(files) = std::fs::read_dir(&session_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    let is_final_wav = path.is_file()
                        && path.extension().map(|e| e == "wav").unwrap_or(false)
                        && !path
                            .file_name()
                            .map(|n| n.to_string_lossy().ends_with(".wav.tmp"))
                            .unwrap_or(false);
                    if !is_final_wav {
                        continue;
                    }
                    let audio_path = path.to_string_lossy().to_string();
                    match db.round_exists_with_audio_path(&audio_path) {
                        Ok(true) => { /* durable evidence — untouched */ }
                        Ok(false) => {
                            // Ambiguous orphaned evidence: preserve by moving
                            // to a SESSION-SCOPED, collision-safe quarantine
                            // destination; never delete (RC-F3).
                            let dest_dir = quarantine.join(&session_name);
                            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                                messages.push(format!(
                                    "[startup-reconcile] cannot create quarantine dir: {e}"
                                ));
                                continue;
                            }
                            let file_name = file.file_name().to_string_lossy().to_string();
                            let mut dest = dest_dir.join(&file_name);
                            let (stem, ext) = split_extension(&file_name);
                            let mut n = 1u32;
                            while dest.exists() {
                                // No-overwrite: never clobber an existing
                                // quarantine artifact.
                                dest = dest_dir.join(format!("{stem}.{n}{ext}"));
                                n += 1;
                            }
                            match std::fs::rename(&path, &dest) {
                                Ok(()) => messages.push(format!(
                                    "[startup-reconcile] quarantined unreferenced evidence: {} -> {}",
                                    path.display(),
                                    dest.display()
                                )),
                                Err(e) => messages.push(format!(
                                    "[startup-reconcile] could not quarantine {} (original left intact): {e}",
                                    path.display()
                                )),
                            }
                        }
                        Err(e) => messages.push(format!(
                            "[startup-reconcile] could not reconcile {}: {e}",
                            path.display()
                        )),
                    }
                }
            }
        }
    }

    // 2. RC-F2: resolve the reconciliation-needed journal. Entries whose
    //    round is now conclusively committed (or conclusively absent) are
    //    resolved and dropped; Unknown/anomalous and malformed entries stay
    //    pending. The journal is rewritten atomically with only the
    //    unresolved subset — a subsequent startup never repeats stale
    //    "pending" warnings for already-resolved entries.
    let journal = quarantine.join("reconciliation-needed.log");
    if let Ok(content) = std::fs::read_to_string(&journal) {
        let mut kept: Vec<&str> = Vec::new();
        let mut resolved = 0usize;
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            match resolve_journal_entry(line, db) {
                JournalResolution::Committed => {
                    resolved += 1;
                    messages.push(format!(
                        "[startup-reconcile] reconciliation resolved (committed): {line}"
                    ));
                }
                JournalResolution::NotCommitted => {
                    resolved += 1;
                    messages.push(format!(
                        "[startup-reconcile] reconciliation resolved (not committed): {line}"
                    ));
                }
                JournalResolution::Unknown(reason) => {
                    kept.push(line);
                    messages.push(format!(
                        "[startup-reconcile] pending reconciliation ({reason}): {line}"
                    ));
                }
                JournalResolution::Malformed => {
                    // Preserved verbatim — never silently discarded.
                    kept.push(line);
                    messages.push(format!(
                        "[startup-reconcile] malformed reconciliation entry preserved: {line}"
                    ));
                }
            }
        }
        if resolved > 0 {
            if rewrite_reconciliation_journal(&journal, &kept) {
                messages.push(format!(
                    "[startup-reconcile] reconciliation journal rewritten: {resolved} resolved, {} pending remain",
                    kept.len()
                ));
            } else {
                messages.push(
                    "[startup-reconcile] could not rewrite reconciliation journal — original preserved"
                        .to_string(),
                );
            }
        }
    }
    messages
}

/// RC-4: durably record an ambiguous-persistence (Unknown COMMIT outcome)
/// entry for startup reconciliation. The evidence is NEVER deleted on an
/// unknown outcome — it is preserved in place and this journal entry makes
/// the reconciliation-needed state durable across restarts. Returns a short
/// user-facing note.
fn record_reconciliation_needed(
    paths: &crate::paths::AppPaths,
    session_id: &str,
    round_index: i32,
    audio_path: &str,
    sha256: &str,
    source: &str,
) -> String {
    let dir = paths.quarantine_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("(could not record reconciliation state: {e})");
    }
    let journal = dir.join("reconciliation-needed.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = format!(
        "{ts} | session={session_id} | round_index={round_index} | audio_path={audio_path} | sha256={sha256} | source={source}\n"
    );
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(entry.as_bytes())
        }) {
        Ok(()) => {
            "Your recording was preserved for review and the case was recorded for startup reconciliation.".to_string()
        }
        Err(e) => format!("(could not record reconciliation state: {e})"),
    }
}

/// RC-1: a round whose WAV already exists must be rejected at the backend
/// round boundary, before any TTS/audio work, so a replayed round_id can
/// never touch (or appear to own) a pre-existing committed WAV.
fn ensure_round_evidence_absent(
    paths: &crate::paths::AppPaths,
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    round_index: i32,
) -> Result<(), String> {
    let round_wav = paths.round_audio_path(session_id, round_id);
    if round_wav.exists() {
        return Err(format!(
            "Round {} already has a recorded answer for this session",
            round_index + 1
        ));
    }
    Ok(())
}

/// Which provisional artifacts a timed-out round actually OWNS (RC-2).
/// Cleanup deletes ONLY what this invocation created — deletion authority
/// is never derived from session/round IDs alone, so a colliding
/// pre-existing committed WAV can never be removed by a timeout.
#[derive(Debug)]
struct RoundArtifactOwnership {
    wav_path: std::path::PathBuf,
    temp_path: std::path::PathBuf,
    /// Invocation-unique transcript temps this round owns (RC-3): files in
    /// `temp/<session>/` named `<round>.txt` (legacy) or starting with
    /// `<round>.` and ending `.txt` (invocation-unique). The round UUID
    /// prefix is unambiguous — no other invocation can share it.
    transcript_paths: Vec<std::path::PathBuf>,
    owns_final: bool,
    owns_temp: bool,
    owns_transcript: bool,
}

impl RoundArtifactOwnership {
    /// Controlled removal (RC-1): returns the count of artifacts that could
    /// NOT be removed (0 = everything owned is confirmed gone). Failures are
    /// logged for reconciliation — success is never claimed silently.
    fn remove_owned(&self) -> usize {
        let mut failed = 0usize;
        let mut candidates: Vec<&std::path::Path> = Vec::new();
        if self.owns_final {
            candidates.push(&self.wav_path);
        }
        if self.owns_temp {
            candidates.push(&self.temp_path);
        }
        if self.owns_transcript {
            candidates.extend(self.transcript_paths.iter().map(|p| p.as_path()));
        }
        for path in candidates {
            let outcome = crate::cleanup::remove_owned(path);
            if outcome.failed() {
                failed += 1;
                eprintln!(
                    "[round-cleanup] {}",
                    crate::cleanup::describe(&outcome, path)
                );
            }
        }
        failed
    }
}

/// Enumerate the invocation-unique transcript temps this round owns (RC-3):
/// any `.txt` in `temp/<session>/` whose name is exactly `<round>.txt`
/// (legacy deterministic form) or starts with `<round>.` (invocation-unique
/// `<round>.<invocation>.txt`). The round-UUID prefix is unambiguous — no
/// other invocation can share it — and a stale file from a crashed attempt
/// of the same logical round is provably owned.
fn owned_transcript_paths(
    paths: &crate::paths::AppPaths,
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
) -> Vec<std::path::PathBuf> {
    let session_dir = paths.temp_dir.join(session_id.hyphenated().to_string());
    let round_prefix = uuid_to_path(&round_id);
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(&session_dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        let is_txt = path.extension().map(|e| e == "txt").unwrap_or(false);
        let round_owned =
            name == format!("{round_prefix}.txt") || name.starts_with(&format!("{round_prefix}."));
        if is_txt && round_owned {
            found.push(path);
        }
    }
    found
}

/// Compute artifact ownership for a timed-out round from the physical
/// capture lifecycle (RC-2). Ownership is NEVER derived from IDs alone:
/// - The final WAV is owned iff THIS invocation's capture FINISHED and the
///   file exists. The round-boundary check (RC-1) rejected any pre-existing
///   final before the worker started, so the only way the final appears is
///   this invocation's temp->final rename; a capture that finished with an
///   error cleaned up its own temp and created no final.
/// - The `.wav.tmp` is owned iff a capture was Scheduled and the file
///   exists (record_to_wav clears stale temps before writing its own).
/// - Transcript temps are owned iff the final WAV is owned — only a
///   successfully recorded round can have started transcription — and the
///   files match this round's invocation-unique prefix (RC-3).
fn round_artifact_ownership(
    paths: &crate::paths::AppPaths,
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    completion: &audio::capture::CaptureCompletion,
) -> RoundArtifactOwnership {
    let wav_path = paths.round_audio_path(session_id, round_id);
    let temp_path = wav_path.with_extension("wav.tmp");
    let capture_attempted = completion.scheduled() || completion.finished();
    let owns_final = completion.finished() && wav_path.exists();
    let transcript_paths = if owns_final {
        owned_transcript_paths(paths, session_id, round_id)
    } else {
        Vec::new()
    };
    RoundArtifactOwnership {
        owns_final,
        owns_temp: capture_attempted && temp_path.exists(),
        owns_transcript: !transcript_paths.is_empty(),
        wav_path,
        temp_path,
        transcript_paths,
    }
}

/// RC-3 tail lifecycle for a timed-out round whose physical capture is
/// STILL ALIVE after the grace period: wait for the real capture
/// termination, THEN recompute artifact ownership (a late temp->final
/// rename makes the final owned by THIS invocation) and remove ONLY owned
/// provisional artifacts (RC-2), THEN release the recording slot. Running
/// the artifact cleanup in the same task that waits for capture completion
/// guarantees a late `.wav.tmp -> .wav` rename can never resurrect an
/// orphan final WAV after the command already returned, and the slot is
/// never freed while physical capture may still run.
async fn deferred_timeout_cleanup(
    completion: audio::capture::CaptureCompletion,
    paths: &crate::paths::AppPaths,
    session_id: uuid::Uuid,
    round_id: uuid::Uuid,
    state: Arc<RecordingState>,
) {
    completion.wait().await;
    let ownership = round_artifact_ownership(paths, session_id, round_id, &completion);
    ownership.remove_owned();
    clear_active_recording(&state).await;
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
    let guard = state.slot.lock().await;
    match &*guard {
        AudioSlotState::Active(handle) => {
            handle.stop();
            Ok("Stop signal sent".to_string())
        }
        // A round that was active when the subsystem was poisoned still owns
        // its stop handle (RC-G1B: poisoning never loses lifecycle
        // ownership) — stopping it remains possible.
        AudioSlotState::Poisoned {
            active: Some(handle),
            ..
        } => {
            handle.stop();
            Ok("Stop signal sent".to_string())
        }
        AudioSlotState::Free | AudioSlotState::Poisoned { active: None, .. } => {
            Err("No active interview round".to_string())
        }
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

/// Typed lookup of a single session by ID (P2-1). The Interview page uses
/// this to VERIFY a Dashboard-handed-off session before any round flow: a
/// missing session returns Ok(None) (stale/invalid handoff), a completed
/// session returns the session with completed_at set, and an unparseable
/// session id is rejected by the Uuid type before this command is reached.
#[tauri::command]
async fn get_session(
    session_id: uuid::Uuid,
    state: State<'_, Arc<DbState>>,
) -> Result<Option<db::InterviewSession>, String> {
    let guard = state.db.lock().await;
    let db = guard.as_ref().ok_or("Database not initialized")?;
    db.get_session(&session_id.hyphenated().to_string())
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

            // 3. RC-1E: startup reconciliation — remove provably-owned stale
            // temporary artifacts (test clips, partial WAVs, transcript and
            // TTS temps) and reconcile candidate EVIDENCE against the DB
            // (unreferenced WAVs are quarantined for review, NEVER deleted;
            // pending reconciliation-needed entries are reported).
            for message in cleanup::reconcile_stale_artifacts(&paths.paths) {
                eprintln!("{}", message);
            }
            for message in reconcile_orphaned_evidence(&paths.paths, &database) {
                eprintln!("{}", message);
            }

            // 4. Manage all states
            app.manage(paths);
            app.manage(Arc::new(RecordingState {
                slot: Mutex::new(AudioSlotState::Free),
            }));
            app.manage(Arc::new(DbState {
                db: Mutex::new(Some(database)),
            }));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_config,
            run_audio_test,
            check_audio_devices,
            run_interview_round,
            retry_interview_round,
            stop_interview_round,
            get_tools_dir,
            create_session,
            get_session,
            get_sessions,
            get_rounds,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio::capture::CaptureState;

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

    /// Wait (bounded) for the guard safety net to clear the recording slot.
    /// Used by RC-G1 ownership tests: the safety net sets poison BEFORE the
    /// clear, so once the slot is empty the poison decision is final.
    async fn wait_for_slot_clear(state: &Arc<RecordingState>) {
        for _ in 0..200 {
            if state.slot_is_released().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("recording slot was not cleared by the guard safety net");
    }

    /// An outer command deadline must return promptly without consuming or
    /// detaching the worker. The still-armed guard retains the recording slot
    /// until the scheduled physical worker actually terminates.
    #[tokio::test]
    async fn recording_worker_deadline_returns_while_guard_retains_ownership() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let mut guard = RecordingGuard::new(state.clone(), stop.clone());
        guard.drop_grace = std::time::Duration::from_millis(50);
        let completion = guard.completion();
        let completion_for_test = completion.clone();
        let blocking_release = release.clone();
        let worker = tokio::spawn(async move {
            completion.mark_scheduled();
            let _ = tokio::task::spawn_blocking(move || {
                while !blocking_release.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                completion.signal();
            })
            .await;
        });
        guard.attach_worker(worker);

        for _ in 0..200 {
            if completion_for_test.scheduled() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(completion_for_test.scheduled());

        let started = std::time::Instant::now();
        let result = take_recording_result_before_deadline(
            &mut guard,
            std::time::Duration::from_millis(25),
            "Audio command timed out",
        )
        .await;
        assert_eq!(result.unwrap_err(), "Audio command timed out");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        drop(guard);
        assert!(stop.load(Ordering::SeqCst));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            state.is_active().await,
            "slot must remain owned while the physical worker is blocked"
        );

        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if state.slot_is_released().await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("slot must clear after physical worker termination");
    }

    /// RC-1 (Audio Test): the test-WAV cleanup lives INSIDE the physical
    /// capture closure (`record_to_wav` with retain_output=false), so
    /// aborting the outer async wrapper on a command timeout can never
    /// destroy a pending cleanup. Models the exact failure topology from the
    /// review: outer worker aborted -> physical capture still alive -> capture
    /// later finalizes the WAV -> the physical closure removes final + tmp
    /// BEFORE signalling Finished -> the slot clears only after termination
    /// AND cleanup, and a retry can start cleanly.
    #[tokio::test]
    async fn audio_test_cleanup_survives_outer_worker_abort() {
        let dir = std::env::temp_dir().join("audio_test_cleanup_abort_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let final_path = dir.join("audio_test_test.wav");
        let tmp_path = final_path.with_extension("wav.tmp");

        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let mut guard = RecordingGuard::new(state.clone(), stop.clone());
        guard.drop_grace = std::time::Duration::from_millis(50);
        let completion = guard.completion();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let completion_for_test = completion.clone();
        let completion_for_cleanup = completion.clone();
        let blocking_release = release.clone();
        let closure_final = final_path.clone();
        let closure_tmp = tmp_path.clone();
        let worker = tokio::spawn(async move {
            // RC-2: mark Scheduled BEFORE spawn_blocking, exactly like
            // record_to_wav.
            completion_for_test.mark_scheduled();
            let closure = tokio::task::spawn_blocking(move || {
                // Physical capture in flight: partial temp exists.
                std::fs::write(&closure_tmp, b"partial").unwrap();
                // The capture ignores the stop flag (hung native call) and
                // continues until the test releases it.
                while !blocking_release.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                // Physical capture finalizes: temp -> final rename.
                std::fs::rename(&closure_tmp, &closure_final).unwrap();
                // RC-1: cleanup runs INSIDE the physical closure
                // (retain_output=false) BEFORE Finished is signalled.
                let _ = std::fs::remove_file(&closure_final);
                completion_for_cleanup.signal();
            });
            let _ = closure.await;
        });
        guard.attach_worker(worker);

        // Command deadline fires (take_recording_result_before_deadline
        // returns Err) -> the command returns with the guard still armed ->
        // the drop safety net runs: stop set, grace (50ms) expires, the outer
        // wrapper is force-aborted, and the physical closure is waited out.
        drop(guard);

        // While the physical capture is still alive, the slot must stay
        // occupied — a retry cannot start.
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(80) {
            assert!(
                state.is_active().await,
                "slot must stay occupied while the physical capture is alive"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        // Physical capture finishes: finalizes the WAV, removes it inside the
        // closure, then signals Finished — only then does the slot clear.
        release.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if state.slot_is_released().await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("slot must clear after physical termination + cleanup");

        assert!(
            !final_path.exists(),
            "final Audio Test WAV must be removed by the physical closure"
        );
        assert!(
            !tmp_path.exists(),
            "no .wav.tmp may survive a timed-out audio test"
        );

        // Retry can start cleanly.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-7: the terminal timeout message is truthful about the cleanup
    /// state — a deferred cleanup, pending output, or failed cleanup never
    /// claims artifacts were already cleaned up; an actually-completed
    /// cleanup reports it.
    #[test]
    fn round_timeout_message_is_truthful() {
        let deferred = round_timeout_message(true, false, false, 300);
        assert!(deferred.contains("Cancellation was requested"));
        assert!(deferred.contains("will be removed after the audio worker exits"));
        assert!(
            !deferred.contains("cleaned up"),
            "deferred cleanup must not claim completion: {}",
            deferred
        );

        let output_shutdown = round_timeout_message(false, true, false, 300);
        assert!(output_shutdown.contains("audio playback is still shutting down"));
        assert!(
            !output_shutdown.contains("cleaned up"),
            "pending output must not claim cleanup: {}",
            output_shutdown
        );

        let failed = round_timeout_message(false, false, true, 300);
        assert!(
            failed.contains("could not be removed and will be reconciled"),
            "failed cleanup must be reported, not claimed complete: {}",
            failed
        );
        assert!(
            !failed.contains("cleaned up"),
            "failed cleanup must not claim completion: {}",
            failed
        );

        let done = round_timeout_message(false, false, false, 300);
        assert!(done.contains("processes terminated, partial artifacts cleaned up"));
        assert!(done.contains("300s"));
    }

    /// Controlled path: after the worker terminates, the command explicitly
    /// clears RecordingState and disarms the guard, so a second round can
    /// acquire the recording slot immediately — no spawned-task timing.
    #[tokio::test]
    async fn recording_slot_reusable_after_explicit_clear_and_disarm() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let handle = fake_handle();

        acquire_recording(&state, handle.clone()).await.unwrap();
        let (guard, _stop) = new_guard(&state);

        // Same ordering as the controlled completion/timeout paths in
        // run_interview_round: worker done -> explicit clear -> disarm.
        clear_active_recording(&state).await;
        guard.disarm();
        drop(guard);

        assert!(state.slot_is_released().await);
        // Second round can acquire the slot without waiting.
        acquire_recording(&state, handle).await.unwrap();
        assert!(state.is_active().await);
    }

    // ------------------------------------------------------------------
    // RC-G1: ReapFailed must fail closed — the audio/process subsystem is
    // poisoned (sticky) and future acquisition is rejected with a
    // restart-required error; Reaped still releases normally.
    // ------------------------------------------------------------------

    /// Test A: a guard whose child lifecycle ended in ReapFailed poisons the
    /// subsystem — the safety net clears the slot (poison is set BEFORE the
    /// clear) and a second acquire is REJECTED with a restart-required error.
    #[tokio::test]
    async fn reap_failed_poisons_subsystem_and_blocks_second_acquire() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();

        // Simulate a child that was Running but whose wait failed: the
        // lifecycle is ReapFailed — Running, then a failed wait.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut guard = RecordingGuard::<()>::new(state.clone(), stop);
        let completion = guard.process_completion();
        completion.mark_running();
        completion.signal_reap_failed();
        // Attach a trivial worker so the Drop safety net runs the process
        // lifecycle path (and not the "nothing scheduled" shortcut).
        guard.attach_worker(tokio::spawn(async {}));
        drop(guard);

        wait_for_slot_clear(&state).await;

        assert!(
            state.poison_reason().await.is_some(),
            "ReapFailed must poison the subsystem"
        );
        let err = acquire_recording(&state, fake_handle()).await.unwrap_err();
        assert!(
            err.contains("Restart the application"),
            "second acquire must be rejected with a restart-required error, got: {err}"
        );
    }

    /// Test B: a conclusive Reaped releases the slot normally and a second
    /// acquire SUCCEEDS — Reaped never poisons.
    #[tokio::test]
    async fn reaped_releases_slot_normally_and_second_acquire_succeeds() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut guard = RecordingGuard::<()>::new(state.clone(), stop);
        let completion = guard.process_completion();
        completion.mark_running();
        completion.signal_reaped();
        guard.attach_worker(tokio::spawn(async {}));
        drop(guard);

        wait_for_slot_clear(&state).await;

        assert!(
            state.poison_reason().await.is_none(),
            "a conclusive Reaped must never poison the subsystem"
        );
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// Test C: the ordinary clear path cannot erase a poisoned state — the
    /// poison is sticky and a second acquire is still rejected.
    #[tokio::test]
    async fn poison_survives_ordinary_clear() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        poison_recording(&state, "simulated reap failure").await;

        // Ordinary release path must NOT erase the poison.
        clear_active_recording(&state).await;
        assert!(
            state.poison_reason().await.is_some(),
            "clear must never erase a poisoned state"
        );
        let err = acquire_recording(&state, fake_handle()).await.unwrap_err();
        assert!(
            err.contains("Restart the application"),
            "still poisoned after clear, got: {err}"
        );
    }

    /// Test D: the round-timeout ReapFailed policy — the extracted helper
    /// surfaces the unresolved lifecycle, poisons the subsystem (bounded: a
    /// terminal ReapFailed resolves immediately), and future acquisition is
    /// rejected.
    #[tokio::test]
    async fn timeout_reap_failed_poisons_and_blocks_future_acquisition() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let completion = audio::pipe::ProcessCompletion::default();
        completion.mark_running();
        completion.signal_reap_failed();

        let unresolved = resolve_process_lifecycle_at_timeout(&state, &completion).await;
        assert!(
            unresolved,
            "ReapFailed at round timeout must be surfaced as an unresolved lifecycle"
        );
        assert!(state.poison_reason().await.is_some());
        let err = acquire_recording(&state, fake_handle()).await.unwrap_err();
        assert!(
            err.contains("Restart the application"),
            "future acquisition must be rejected after a timeout ReapFailed, got: {err}"
        );
    }

    /// Round-timeout counterpart: a conclusive Reaped at timeout is normal and
    /// must NOT poison — the slot stays reusable.
    #[tokio::test]
    async fn timeout_reaped_does_not_poison() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let completion = audio::pipe::ProcessCompletion::default();
        completion.mark_running();
        completion.signal_reaped();

        let unresolved = resolve_process_lifecycle_at_timeout(&state, &completion).await;
        assert!(!unresolved);
        assert!(state.poison_reason().await.is_none());
        acquire_recording(&state, fake_handle()).await.unwrap();
    }

    // ------------------------------------------------------------------
    // RC-G1A: NORMAL (non-timeout) worker completion must resolve the native
    // child lifecycle before any reusable release — same shared policy as
    // timeout and Drop, exercised through the guard mechanics the real
    // normal-completion path uses (take_result -> resolve -> clear -> disarm).
    // ------------------------------------------------------------------

    /// G1A-1: a worker that finishes before the deadline with a ReapFailed
    /// lifecycle poisons the subsystem; the safe release runs; a second
    /// acquire is REJECTED with the restart-required error.
    #[tokio::test]
    async fn normal_completion_reap_failed_poisons_and_blocks_second_acquire() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();
        let mut guard = new_guard(&state).0;
        let completion = guard.process_completion();
        completion.mark_running();
        completion.signal_reap_failed();
        // The worker completed before the deadline (trivial, already done).
        guard.attach_worker(tokio::spawn(async {}));

        // Normal-completion release order: take result -> resolve lifecycle
        // -> (persistence) -> clear -> disarm.
        let _ = guard.take_result().await;
        let err = resolve_process_completion(&state, &completion, "round completion")
            .await
            .unwrap_err();
        assert!(
            err.contains("Restart the application"),
            "restart-required error must be surfaced, got: {err}"
        );
        clear_active_recording(&state).await;
        guard.disarm();

        assert!(state.poison_reason().await.is_some());
        assert!(state.slot_is_released().await);
        let second = acquire_recording(&state, fake_handle()).await.unwrap_err();
        assert!(
            second.contains("Restart the application"),
            "second acquire after normal-completion ReapFailed must be rejected, got: {second}"
        );
    }

    /// G1A-2: a conclusive Reaped on normal completion releases normally —
    /// no poison, second acquire succeeds.
    #[tokio::test]
    async fn normal_completion_reaped_releases_normally() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();
        let mut guard = new_guard(&state).0;
        let completion = guard.process_completion();
        completion.mark_running();
        completion.signal_reaped();
        guard.attach_worker(tokio::spawn(async {}));

        let _ = guard.take_result().await;
        resolve_process_completion(&state, &completion, "round completion")
            .await
            .unwrap();
        clear_active_recording(&state).await;
        guard.disarm();

        assert!(state.poison_reason().await.is_none());
        assert!(state.slot_is_free().await);
        acquire_recording(&state, fake_handle()).await.unwrap();
    }

    /// G1A-3: a worker that returns an ordinary error while the lifecycle is
    /// ReapFailed — the original cause is preserved and the stronger
    /// restart-required condition is appended; the subsystem is poisoned.
    #[tokio::test]
    async fn normal_worker_error_with_reap_failed_preserves_cause_and_poisons() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut guard = RecordingGuard::<Result<(), String>>::new(state.clone(), stop);
        let completion = guard.process_completion();
        completion.mark_running();
        completion.signal_reap_failed();
        let worker = tokio::spawn(async {
            Err::<(), String>("Piper wait error: child exited abnormally".to_string())
        });
        guard.attach_worker(worker);

        let join_result = guard.take_result().await;
        let result = match join_result {
            Some(Ok(inner)) => inner.map_err(|e| e.to_string()),
            Some(Err(join_err)) => Err(format!("Interview worker failed: {}", join_err)),
            None => Err("Interview worker handle lost".to_string()),
        };
        let lifecycle_error = resolve_process_completion(&state, &completion, "round completion")
            .await
            .unwrap_err();

        // Normal-completion error branch composition: original cause +
        // restart-required lifecycle condition.
        let original = result.unwrap_err();
        let message = format!("{original}. {lifecycle_error}");
        assert!(
            message.contains("Piper wait error"),
            "original cause must be preserved: {message}"
        );
        assert!(
            message.contains("Restart the application"),
            "restart-required condition must be surfaced: {message}"
        );
        assert!(state.poison_reason().await.is_some());
        clear_active_recording(&state).await;
        guard.disarm();
        assert!(acquire_recording(&state, fake_handle()).await.is_err());
    }

    // ------------------------------------------------------------------
    // RC-G1B: poison check + slot acquisition are ONE atomic transition under
    // one mutex — deterministic concurrency regressions, no timing-only
    // sleeps.
    // ------------------------------------------------------------------

    /// G1B-1/G1B-2: a deterministic barrier forces a concurrent acquire and
    /// poison. In EVERY interleaving the final state is Poisoned: either the
    /// acquire was rejected, or it was admitted BEFORE the poison transition
    /// and its handle is retained INSIDE the Poisoned state — never an
    /// admitted operation outside the poison boundary.
    #[tokio::test]
    async fn poison_and_acquire_race_never_admits_operation_after_poison() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let a_state = state.clone();
        let a_bar = barrier.clone();
        let a = tokio::spawn(async move {
            a_bar.wait().await;
            acquire_recording(&a_state, fake_handle()).await
        });
        let b_state = state.clone();
        let b_bar = barrier.clone();
        let b = tokio::spawn(async move {
            b_bar.wait().await;
            poison_recording(&b_state, "simulated reap failure").await;
        });

        barrier.wait().await;
        let a_result = a.await.unwrap();
        b.await.unwrap();

        assert!(
            state.poison_reason().await.is_some(),
            "poison must win the race in every interleaving"
        );
        match a_result {
            Ok(()) => {
                // Admitted before the poison transition — the handle must be
                // retained inside Poisoned, not left as an un-poisoned Active.
                let slot = &*state.slot.lock().await;
                assert!(
                    matches!(
                        slot,
                        AudioSlotState::Poisoned {
                            active: Some(_),
                            ..
                        }
                    ),
                    "admitted handle must be retained inside Poisoned"
                );
            }
            Err(e) => {
                assert!(e.contains("Restart the application"), "got: {e}");
                assert!(state.slot_is_released().await);
            }
        }
        // A NEW acquisition after the race is impossible.
        assert!(acquire_recording(&state, fake_handle()).await.is_err());
    }

    /// G1B-1 deterministic "poison wins" ordering: an acquire that is already
    /// parked on the slot mutex while the poison transition completes under
    /// that SAME held critical section must be rejected — the historical
    /// check-then-act TOCTOU has no window in the single-lock design. The
    /// test applies the poison transition directly under the held lock
    /// (mirroring `poison_recording`'s transition) because the lock is held
    /// by the test itself.
    #[tokio::test]
    async fn acquire_parked_before_poison_cannot_cross_poison_boundary() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        // Hold the slot lock so the acquire parks on the mutex.
        let mut held = state.slot.lock().await;

        let a_state = state.clone();
        let a = tokio::spawn(async move { acquire_recording(&a_state, fake_handle()).await });
        // Let the acquire start and park on the mutex (it cannot proceed —
        // the lock is held).
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // The poison transition happens under the SAME critical section the
        // parked acquire is blocked on.
        match &mut *held {
            AudioSlotState::Free => {
                *held = AudioSlotState::Poisoned {
                    active: None,
                    reason: "simulated reap failure".to_string(),
                }
            }
            _ => unreachable!("slot started Free"),
        }
        drop(held);

        // When the acquire finally obtains the lock it sees Poisoned and is
        // rejected — it cannot install an operation across the poison
        // boundary.
        let err = a.await.unwrap().unwrap_err();
        assert!(
            err.contains("Restart the application"),
            "parked acquire must be rejected after the poison transition, got: {err}"
        );
        assert!(state.poison_reason().await.is_some());
        assert!(state.slot_is_released().await);
    }

    /// Safety net: a guard dropped while still armed (panic/cancellation path)
    /// must still clear the slot.
    #[tokio::test]
    async fn recording_slot_cleared_by_guard_safety_net_on_drop() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let handle = fake_handle();
        acquire_recording(&state, handle.clone()).await.unwrap();

        {
            let (guard, _stop) = new_guard(&state);
            drop(guard);
        }
        // Let the spawned safety-net clear task run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(state.slot_is_released().await);
    }

    /// P2-1: a guard dropped while armed with a live worker sets the stop
    /// flag, terminates the worker, and clears the slot ONLY after
    /// termination — the slot can never be freed while a detached worker is
    /// still alive (a bare JoinHandle drop would detach the task).
    #[tokio::test]
    async fn recording_guard_drop_with_live_worker_terminates_before_clearing() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
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

        // Give the drop-spawned cleanup task time to run (no-capture worker:
        // the capture stays NotScheduled, so the cleanup aborts the outer
        // wrapper and clears the slot).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert!(
            stop.load(Ordering::SeqCst),
            "drop must set the stop flag so the worker terminates"
        );
        assert!(
            state.slot_is_released().await,
            "slot must be cleared only after the worker terminated"
        );
    }

    /// P1-1 cooperative case: the drop safety net uses the REAL nested
    /// topology (outer tokio::spawn -> spawn_blocking -> loops until stop
    /// observed). Dropping an armed guard sets the stop flag; the nested
    /// blocking worker exits within the cooperative grace and signals
    /// completion (exactly like record_to_wav's lifecycle guard), and only
    /// then is the slot cleared and reusable. A second acquire fails while
    /// the blocking worker is still alive.
    #[tokio::test]
    async fn recording_guard_drop_waits_for_nested_blocking_worker() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let stop_seen = Arc::new(tokio::sync::Notify::new());

        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            let completion = guard.completion();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_release = release.clone();
                let stop_seen = stop_seen.clone();
                let completion = completion.clone();
                tokio::spawn(async move {
                    // Outer async worker awaiting the nested blocking capture.
                    // RC-2: mark Scheduled BEFORE spawn_blocking — no await
                    // between, exactly like record_to_wav.
                    completion.mark_scheduled();
                    let blocking = tokio::task::spawn_blocking(move || {
                        // Mimic record_to_wav's physical capture: loop until
                        // stop observed, then stay "still finishing" until
                        // released.
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        // Stop observed — signal it.
                        stop_seen.notify_waiters();
                        while !blocking_release.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        // Blocking worker fully terminated — signal completion,
                        // exactly like BlockingLifecycle.
                        completion.signal();
                    });
                    let _ = blocking.await;
                })
            };
            guard.attach_worker(worker);
            // Dropped while armed — unexpected drop/panic of the outer command.
        }

        // 1. Drop set the stop flag and the nested blocking worker observed it.
        tokio::time::timeout(std::time::Duration::from_secs(5), stop_seen.notified())
            .await
            .expect("drop must set stop and the blocking worker must observe it");
        assert!(stop.load(Ordering::SeqCst), "drop must set the stop flag");

        // 2-3. Blocking worker still alive -> slot still occupied; a second
        // acquire must fail.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            state.is_active().await,
            "slot must stay occupied while the nested blocking worker is alive"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "second acquire must not succeed while the cancelled worker is alive"
        );

        // 4. Release the blocking worker -> it terminates -> signals
        // completion -> cleanup clears the slot.
        release.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..200 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must be cleared only after worker termination"
        );

        // 5. Slot is reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// P1-1 force-abort regression: a nested blocking capture that survives
    /// BEYOND the cooperative grace does NOT free the slot early. The outer
    /// wrapper is force-aborted (which cannot kill the spawn_blocking
    /// closure), but the slot stays occupied until the REAL blocking worker
    /// terminates and signals completion; only then is it cleared and
    /// reusable. The drop_grace is shrunk so the test is fast.
    #[tokio::test]
    async fn recording_guard_drop_force_abort_does_not_free_slot_early() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let stop_seen = Arc::new(tokio::sync::Notify::new());

        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            // Shrink the drop-safety-net grace so the force-abort path is
            // exercised quickly (production default is 5s).
            guard.drop_grace = std::time::Duration::from_millis(150);
            let completion = guard.completion();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_release = release.clone();
                let stop_seen = stop_seen.clone();
                let completion = completion.clone();
                tokio::spawn(async move {
                    // Real nested topology: outer tokio::spawn -> spawn_blocking.
                    // RC-2: mark Scheduled BEFORE spawn_blocking.
                    completion.mark_scheduled();
                    let blocking = tokio::task::spawn_blocking(move || {
                        // 1. Observe stop.
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        // 2. Signal stop was observed.
                        stop_seen.notify_waiters();
                        // 3. Remain alive beyond the cooperative grace.
                        while !blocking_release.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        // 4. Terminate only after the test releases it.
                        completion.signal();
                    });
                    let _ = blocking.await;
                })
            };
            guard.attach_worker(worker);
            // Dropped while armed — unexpected drop/panic of the outer command.
        }

        // The blocking worker observed stop.
        tokio::time::timeout(std::time::Duration::from_secs(5), stop_seen.notified())
            .await
            .expect("blocking worker must observe the drop-set stop flag");
        assert!(stop.load(Ordering::SeqCst), "drop must set the stop flag");

        // Wait PAST the shrunk grace AND the outer abort: the real blocking
        // worker is still alive, so the slot must still be occupied and a
        // second acquire must fail.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            state.is_active().await,
            "slot must stay occupied past the grace/abort while the blocking worker is alive"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "force-aborting the outer wrapper must not free the slot early"
        );

        // Release the blocking worker -> it signals completion -> the cleanup
        // clears the slot ONLY then.
        release.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..200 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after real blocking-worker termination"
        );

        // Slot is reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-2 regression: the capture lifecycle is explicitly tri-state
    /// (NotScheduled -> Scheduled -> Finished) and `mark_scheduled()` runs
    /// BEFORE `spawn_blocking` with no await in between. A capture that is
    /// Scheduled but whose blocking closure has NOT begun executing (queued
    /// on a busy blocking pool, modelled here by a gate) must keep the slot
    /// occupied through grace expiry AND force-abort of the outer wrapper;
    /// the slot frees only after the real closure runs to termination and
    /// signals Finished. There is no timing assumption about when the
    /// closure "should" have started — the gate proves it never began.
    #[tokio::test]
    async fn recording_guard_drop_scheduled_but_not_started_holds_slot() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The blocking closure cannot BEGIN executing (and therefore cannot
        // signal Finished) until the test opens the gate.
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let began = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let completion = guard.completion();
            completion_for_test = completion.clone();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_gate = gate.clone();
                let blocking_began = began.clone();
                let completion = completion.clone();
                tokio::spawn(async move {
                    // RC-2: Scheduled BEFORE spawn_blocking — no await between.
                    completion.mark_scheduled();
                    let blocking = tokio::task::spawn_blocking(move || {
                        // Simulate a queued closure: it may not begin
                        // executing until the gate opens.
                        while !blocking_gate.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        blocking_began.store(true, Ordering::SeqCst);
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        // Fully terminated — signal completion.
                        completion.signal();
                    });
                    let _ = blocking.await;
                })
            };
            guard.attach_worker(worker);

            // Wait until the capture is EXPLICITLY Scheduled (submitted but
            // closure not started) before dropping — the state this
            // regression targets. The gate guarantees the closure never
            // began during this window.
            let mut scheduled = false;
            for _ in 0..200 {
                if completion_for_test.state() == CaptureState::Scheduled {
                    scheduled = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(scheduled, "capture must reach the explicit Scheduled state");
            assert_eq!(completion_for_test.state(), CaptureState::Scheduled);
            assert!(
                !began.load(Ordering::SeqCst),
                "blocking closure must not have begun while gated"
            );

            // Dropped while armed — unexpected drop/panic of the outer
            // command while the capture is scheduled-but-not-started.
        }

        // The drop safety net set stop, ran the shrunk grace (150ms), and
        // force-aborted the outer wrapper; it is now waiting out the real
        // blocking closure, which is STILL gated (never began, never
        // signalled Finished). The slot must remain occupied and a second
        // acquire must fail.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert_eq!(
            completion_for_test.state(),
            CaptureState::Scheduled,
            "scheduled-but-not-finished capture must remain Scheduled after outer abort"
        );
        assert!(
            !began.load(Ordering::SeqCst),
            "closure must still be gated (never started) while the slot is held"
        );
        assert!(
            state.is_active().await,
            "slot must stay occupied while a scheduled capture has not terminated"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "second acquire must fail while the scheduled capture is unresolved"
        );

        // Open the gate: the closure begins, observes stop, terminates, and
        // signals Finished. ONLY then may the slot clear.
        gate.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after real blocking-closure termination"
        );
        assert_eq!(completion_for_test.state(), CaptureState::Finished);

        // Reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-3 regression: a timed-out round whose physical capture is still
    /// alive must finish in the order real capture termination -> provisional
    /// artifact cleanup -> recording slot release. While the capture has not
    /// signalled completion, the final WAV, `.wav.tmp` and transcript temp
    /// all survive AND the slot stays occupied; only after real termination
    /// are the artifacts removed and the slot released — a late
    /// `.wav.tmp -> .wav` rename can never resurrect an orphan final WAV
    /// after the command already returned.
    #[tokio::test]
    async fn deferred_timeout_cleanup_orders_capture_artifacts_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            tmp.path().join("tools"),
            tmp.path().join("data"),
        )
        .unwrap();

        let session_id = uuid::Uuid::new_v4();
        let round_id = uuid::Uuid::new_v4();
        let wav_path = paths.round_audio_path(session_id, round_id);
        std::fs::create_dir_all(wav_path.parent().unwrap()).unwrap();
        // A late rename would target this final path — it must never survive.
        std::fs::write(&wav_path, b"late-resurrected").unwrap();
        std::fs::write(wav_path.with_extension("wav.tmp"), b"partial").unwrap();
        let transcript_path = paths
            .temp_dir
            .join(session_id.hyphenated().to_string())
            .join(format!("{}.txt", uuid_to_path(&round_id)));
        std::fs::create_dir_all(transcript_path.parent().unwrap()).unwrap();
        std::fs::write(&transcript_path, b"partial").unwrap();

        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        acquire_recording(&state, fake_handle()).await.unwrap();

        let completion = audio::capture::CaptureCompletion::new();
        let signal = completion.clone();
        let paths_clone = paths.clone();
        let state_clone = state.clone();
        let cleanup = tokio::spawn(async move {
            deferred_timeout_cleanup(completion, &paths_clone, session_id, round_id, state_clone)
                .await;
        });

        // While the physical capture is still alive (no completion signal):
        // artifacts survive and the slot stays occupied.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            wav_path.exists(),
            "final WAV must survive until capture termination"
        );
        assert!(
            wav_path.with_extension("wav.tmp").exists(),
            "temp must survive until capture termination"
        );
        assert!(
            transcript_path.exists(),
            "transcript temp must survive until capture termination"
        );
        assert!(
            state.is_active().await,
            "slot must stay occupied until capture termination"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err(),
            "second acquire must fail while the capture is unresolved"
        );

        // Real capture terminates -> artifacts removed -> slot released.
        signal.signal();
        tokio::time::timeout(std::time::Duration::from_secs(5), cleanup)
            .await
            .expect("deferred cleanup must finish")
            .expect("cleanup task must not panic");
        assert!(
            !wav_path.exists(),
            "final WAV must be removed after capture termination"
        );
        assert!(
            !wav_path.with_extension("wav.tmp").exists(),
            "temp must be removed after capture termination"
        );
        assert!(
            !transcript_path.exists(),
            "transcript temp must be removed after capture termination"
        );
        assert!(
            state.slot_is_released().await,
            "slot must be released only after cleanup"
        );

        // Reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-2: round-artifact ownership is derived from the capture lifecycle,
    /// NEVER from session/round IDs alone. A pre-existing committed WAV next
    /// to a capture that never finished (e.g. TTS hang) is NOT owned and
    /// survives cleanup; a capture that finished owns the final it created;
    /// a scheduled-but-unfinished capture owns its temp (and its transcript
    /// temp only when the final is owned).
    #[test]
    fn round_artifact_ownership_never_deletes_unowned_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            tmp.path().join("tools"),
            tmp.path().join("data"),
        )
        .unwrap();

        // Case 1: capture NEVER scheduled (TTS hang) + pre-existing WAV.
        let session = uuid::Uuid::new_v4();
        let round = uuid::Uuid::new_v4();
        let wav = paths.round_audio_path(session, round);
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        let original = b"pre-existing-committed-evidence";
        std::fs::write(&wav, original).unwrap();

        let completion = audio::capture::CaptureCompletion::new();
        let ownership = round_artifact_ownership(&paths, session, round, &completion);
        assert!(!ownership.owns_final, "unstarted capture owns no final");
        assert!(!ownership.owns_temp, "unstarted capture owns no temp");
        ownership.remove_owned();
        assert_eq!(
            std::fs::read(&wav).unwrap(),
            original,
            "pre-existing committed WAV must survive an unowned timeout cleanup"
        );

        // Case 2: capture FINISHED -> owns the final it created, and the
        // round transcript temp (only because the final is owned).
        let session2 = uuid::Uuid::new_v4();
        let round2 = uuid::Uuid::new_v4();
        let wav2 = paths.round_audio_path(session2, round2);
        std::fs::create_dir_all(wav2.parent().unwrap()).unwrap();
        std::fs::write(&wav2, b"created-by-this-invocation").unwrap();
        let transcript2 = paths
            .temp_dir
            .join(session2.hyphenated().to_string())
            .join(format!("{}.txt", uuid_to_path(&round2)));
        std::fs::create_dir_all(transcript2.parent().unwrap()).unwrap();
        std::fs::write(&transcript2, b"partial").unwrap();

        let completion2 = audio::capture::CaptureCompletion::new();
        completion2.mark_scheduled();
        completion2.signal();
        let ownership2 = round_artifact_ownership(&paths, session2, round2, &completion2);
        assert!(ownership2.owns_final, "finished capture owns its final");
        assert!(
            ownership2.owns_transcript,
            "owned final implies owned transcript"
        );
        ownership2.remove_owned();
        assert!(!wav2.exists(), "owned final must be removed");
        assert!(
            !transcript2.exists(),
            "owned transcript temp must be removed"
        );

        // Case 3: capture Scheduled but NOT finished -> owns its temp, never
        // a final.
        let session3 = uuid::Uuid::new_v4();
        let round3 = uuid::Uuid::new_v4();
        let wav3 = paths.round_audio_path(session3, round3);
        std::fs::create_dir_all(wav3.parent().unwrap()).unwrap();
        std::fs::write(wav3.with_extension("wav.tmp"), b"partial").unwrap();

        let completion3 = audio::capture::CaptureCompletion::new();
        completion3.mark_scheduled();
        let ownership3 = round_artifact_ownership(&paths, session3, round3, &completion3);
        assert!(!ownership3.owns_final, "unfinished capture owns no final");
        assert!(ownership3.owns_temp, "scheduled capture owns its temp");
        ownership3.remove_owned();
        assert!(
            !wav3.with_extension("wav.tmp").exists(),
            "owned temp must be removed"
        );
    }

    /// RC-1: a round whose WAV already exists is rejected at the backend
    /// round boundary — a replayed round_id can never touch a pre-existing
    /// committed WAV.
    #[test]
    fn ensure_round_evidence_absent_rejects_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            tmp.path().join("tools"),
            tmp.path().join("data"),
        )
        .unwrap();
        let session_id = uuid::Uuid::new_v4();
        let round_id = uuid::Uuid::new_v4();
        let wav = paths.round_audio_path(session_id, round_id);
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"committed-evidence").unwrap();

        let rejected = ensure_round_evidence_absent(&paths, session_id, round_id, 2);
        assert!(rejected.is_err(), "colliding round WAV must be rejected");
        assert!(
            rejected
                .unwrap_err()
                .contains("already has a recorded answer"),
            "collision must be reported explicitly"
        );

        let fresh_round = uuid::Uuid::new_v4();
        assert!(
            ensure_round_evidence_absent(&paths, session_id, fresh_round, 2).is_ok(),
            "a fresh round_id must pass the boundary check"
        );
    }

    /// RC-7: when the physical capture completes WITHIN the cooperative
    /// grace, the drop cleanup must still await the owned OUTER worker
    /// before clearing the slot. Physical completion first, outer wrapper
    /// deliberately delayed: the slot must not clear until the wrapper is
    /// resolved.
    #[tokio::test]
    async fn recording_guard_drop_awaits_outer_worker_after_physical_completion() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let physical_done = Arc::new(tokio::sync::Notify::new());
        let outer_done = Arc::new(tokio::sync::Notify::new());
        acquire_recording(&state, fake_handle()).await.unwrap();

        let completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            // Generous grace: the physical capture completes within it.
            guard.drop_grace = std::time::Duration::from_secs(5);
            let completion = guard.completion();
            completion_for_test = completion.clone();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_release = release.clone();
                let physical_done = physical_done.clone();
                let outer_done = outer_done.clone();
                let completion = completion.clone();
                tokio::spawn(async move {
                    completion.mark_scheduled();
                    let blocking = tokio::task::spawn_blocking(move || {
                        // Physical capture: observe stop quickly, signal
                        // physical completion, then wait to be released.
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        physical_done.notify_waiters();
                        while !blocking_release.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                        completion.signal();
                    });
                    let _ = blocking.await;
                    // The OUTER wrapper deliberately delays AFTER the
                    // physical capture completed — the slot must not clear
                    // until this resolves (RC-7).
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    outer_done.notify_waiters();
                })
            };
            guard.attach_worker(worker);
            // Dropped while armed — unexpected drop/panic of the outer command.
        }

        // Physical capture completed within grace.
        tokio::time::timeout(std::time::Duration::from_secs(5), physical_done.notified())
            .await
            .expect("physical capture must observe stop and complete");
        release.store(true, Ordering::SeqCst);
        // Wait until the physical capture signalled Finished.
        let mut finished = false;
        for _ in 0..200 {
            if completion_for_test.finished() {
                finished = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            finished,
            "physical capture must signal Finished within grace"
        );

        // The outer wrapper is still delaying -> the slot must STILL be
        // occupied (physical completion alone must not free it).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            state.is_active().await,
            "slot must stay occupied until the outer worker is resolved"
        );

        // Outer worker resolves -> cleanup clears the slot only then.
        tokio::time::timeout(std::time::Duration::from_secs(5), outer_done.notified())
            .await
            .expect("outer worker must resolve");
        let mut cleared = false;
        for _ in 0..200 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(cleared, "slot must clear after the outer worker resolves");

        // Reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-2: the SPEAKER probe (output probe) lifecycle is tracked by its own
    /// CaptureCompletion. If the outer device-check task is cancelled while
    /// the speaker probe is inside spawn_blocking, the blocking output worker
    /// can outlive the outer worker — the slot must stay occupied until the
    /// real output probe signals Finished, and becomes reusable only then
    /// (no audio slot reuse until the probe physically ends, RC-3).
    #[tokio::test]
    async fn recording_guard_drop_waits_for_speaker_probe_output_completion() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The probe cannot BEGIN executing (and therefore cannot signal
        // Finished) until the test opens the gate.
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let began = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let output_completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let output_completion = guard.output_completion();
            output_completion_for_test = output_completion.clone();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_gate = gate.clone();
                let blocking_began = began.clone();
                let output_completion = output_completion.clone();
                tokio::spawn(async move {
                    // The device-check worker starts the speaker probe:
                    // mark_scheduled BEFORE spawn_blocking (RC-2).
                    output_completion.mark_scheduled();
                    let probe = tokio::task::spawn_blocking(move || {
                        // Simulate a queued output probe: it may not begin
                        // (and cannot signal Finished) until the gate opens.
                        while !blocking_gate.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        blocking_began.store(true, Ordering::SeqCst);
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        // Output probe fully terminated — signal completion.
                        output_completion.signal();
                    });
                    let _ = probe.await;
                })
            };
            guard.attach_worker(worker);

            // Wait until the speaker probe is EXPLICITLY Scheduled (submitted
            // but not started) before dropping.
            let mut scheduled = false;
            for _ in 0..200 {
                if output_completion_for_test.state() == CaptureState::Scheduled {
                    scheduled = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(scheduled, "speaker probe must reach Scheduled before drop");
            // Dropped while armed — cancellation of the outer device-check
            // command while the speaker probe is queued/running.
        }

        // Grace (150ms) expired, outer wrapper aborted and awaited; the
        // speaker probe is still gated (never began, never signalled
        // Finished) -> the slot must STILL be occupied and a second acquire
        // must fail.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !began.load(Ordering::SeqCst),
            "speaker probe must still be gated while the slot is held"
        );
        assert!(
            state.is_active().await,
            "slot must stay occupied while the speaker probe is unresolved"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "second acquire must fail while the speaker probe is unresolved"
        );

        // Open the gate: the probe begins, observes stop, terminates, and
        // signals Finished. ONLY then may the slot clear (RC-3: no slot
        // reuse until the probe physically ends).
        gate.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after the speaker probe physically ends"
        );
        assert_eq!(output_completion_for_test.state(), CaptureState::Finished);

        // Reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-2A regression: the Drop safety net MUST re-read every physical
    /// lifecycle tracker AFTER the outer worker is resolved. Model the exact
    /// review topology: the initial snapshot contains ONLY the input probe; a
    /// second physical worker (the output probe) becomes Scheduled after that
    /// snapshot (the device check schedules it as the input probe finishes)
    /// while the guard's cleanup is still running. The slot must stay
    /// occupied until the late-scheduled output finishes — a pre-abort
    /// snapshot is NOT authoritative.
    #[tokio::test]
    async fn recording_guard_drop_rescans_completions_after_snapshot() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let input_release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let schedule_output = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let output_release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let completion_for_test;
        let output_completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let completion = guard.completion();
            let output_completion = guard.output_completion();
            completion_for_test = completion.clone();
            output_completion_for_test = output_completion.clone();
            let worker = {
                let blocking_input_release = input_release.clone();
                let blocking_schedule = schedule_output.clone();
                let blocking_output_release = output_release.clone();
                let completion = completion.clone();
                let output_completion = output_completion.clone();
                tokio::spawn(async move {
                    // Input probe: Scheduled immediately (RC-2 ordering).
                    completion.mark_scheduled();
                    let input = tokio::task::spawn_blocking(move || {
                        while !blocking_input_release.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        completion.signal();
                    });
                    // The device-check continuation: once the input finishes,
                    // the NEXT probe (output) is scheduled. Modelled as a
                    // child task — the outcome is the same as the
                    // grace-boundary race where the output probe is submitted
                    // just before the outer abort lands.
                    let sched = blocking_schedule.clone();
                    let out_completion = output_completion.clone();
                    let out_release = blocking_output_release.clone();
                    tokio::spawn(async move {
                        while !sched.load(Ordering::SeqCst) {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                        // The output probe becomes Scheduled AFTER the guard's
                        // initial snapshot.
                        out_completion.mark_scheduled();
                        let output = tokio::task::spawn_blocking(move || {
                            while !out_release.load(Ordering::SeqCst) {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            out_completion.signal();
                        });
                        let _ = output.await;
                    });
                    let _ = input.await;
                })
            };
            guard.attach_worker(worker);

            // Wait until the input probe is Scheduled (the snapshot the Drop
            // will take) before dropping.
            let mut scheduled = false;
            for _ in 0..200 {
                if completion_for_test.state() == CaptureState::Scheduled {
                    scheduled = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(scheduled, "input probe must be Scheduled before drop");
            assert!(
                !output_completion_for_test.scheduled(),
                "output probe must NOT be Scheduled at the initial snapshot"
            );
            // Dropped while armed — cancellation of the outer device-check
            // command while the input probe is scheduled.
        }

        // Schedule the output probe AFTER the drop's initial snapshot (the
        // grace-boundary race), then release the input probe.
        schedule_output.store(true, Ordering::SeqCst);
        input_release.store(true, Ordering::SeqCst);

        // Give the drop cleanup time to resolve the outer worker and run its
        // rescan. The late-scheduled output probe must keep the slot
        // occupied.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(
            matches!(
                output_completion_for_test.state(),
                CaptureState::Scheduled | CaptureState::Finished
            ),
            "output probe must have been scheduled by now"
        );
        assert!(
            state.is_active().await,
            "slot must stay occupied while the late-scheduled output probe is unresolved"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err(),
            "second acquire must fail while the output probe is unresolved"
        );

        // Release the output probe -> it signals Finished -> only then does
        // the slot clear.
        output_release.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after the late-scheduled output probe finishes"
        );
        assert_eq!(output_completion_for_test.state(), CaptureState::Finished);
    }

    /// RC-2B: the Drop safety net waits for a Running native child to be
    /// Reaped before clearing the slot — a force-abort of the outer worker
    /// can never release ownership while a Piper/Whisper process may be
    /// alive.
    #[tokio::test]
    async fn recording_guard_drop_waits_for_running_process() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let process_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let process_completion = guard.process_completion();
            process_for_test = process_completion.clone();
            let worker = tokio::spawn(async move {
                // The worker marks a child Running (Piper/Whisper spawn) and
                // is then force-aborted while the child is still alive — it
                // never reaps the child itself.
                process_completion.mark_running();
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            });
            guard.attach_worker(worker);

            let mut running = false;
            for _ in 0..200 {
                if process_for_test.running() {
                    running = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(running, "process must be Running before drop");
            // Dropped while armed — the owning round command is cancelled
            // while a native child may be alive.
        }

        // The outer worker was force-aborted, but the child is not yet
        // reaped — the slot must stay occupied and a second acquire must
        // fail.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            state.is_active().await,
            "slot must stay occupied while a child may be running"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err(),
            "second acquire must fail while the child is unreaped"
        );

        // The child is conclusively reaped (in the real flow, the aborted
        // task's own RAII guard kills + waits and then signals Reaped) —
        // only now may the slot clear.
        process_for_test.signal_reaped();
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(cleared, "slot must clear only after the process is Reaped");
    }

    // ------------------------------------------------------------------
    // RC-1: PRODUCTION interview TTS playback is owned by
    // RecordingGuard.output_completion — the real Piper question-playback
    // worker (play_raw_pcm_async's spawn_blocking) marks the output
    // lifecycle Scheduled before submission and signals Finished on every
    // exit. These tests model that worker through the guard's output
    // completion: the shared audio slot must stay occupied while it is
    // Scheduled, and become reusable only after Finished.
    // ------------------------------------------------------------------

    /// RC-1 Test A — production output scheduled but not started: the
    /// interview worker has submitted the production TTS blocking output
    /// (Scheduled) but the closure is gated before actual execution. The
    /// owning command is then dropped. The slot must remain occupied, a
    /// second acquire must fail, and force-aborting the outer wrapper must
    /// NOT free the slot; only when the blocking output worker finally runs
    /// to termination and signals Finished does the slot clear.
    #[tokio::test]
    async fn recording_guard_drop_waits_for_production_tts_output_scheduled_but_not_started() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The production output closure cannot BEGIN executing (and therefore
        // cannot signal Finished) until the test opens the gate.
        let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let began = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let output_completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let output_completion = guard.output_completion();
            output_completion_for_test = output_completion.clone();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_gate = gate.clone();
                let blocking_began = began.clone();
                let output_completion = output_completion.clone();
                tokio::spawn(async move {
                    // The interview worker starts the PRODUCTION TTS playback:
                    // play_raw_pcm_async marks Scheduled BEFORE spawn_blocking
                    // (RC-1), with no await in between.
                    output_completion.mark_scheduled();
                    let playback = tokio::task::spawn_blocking(move || {
                        // Simulate a queued output worker: it may not begin
                        // (and cannot signal Finished) until the gate opens.
                        while !blocking_gate.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        blocking_began.store(true, Ordering::SeqCst);
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        // Physical output worker fully exited — signal
                        // Finished, exactly like BlockingLifecycle.
                        output_completion.signal();
                    });
                    let _ = playback.await;
                })
            };
            guard.attach_worker(worker);

            // Wait until the production output is EXPLICITLY Scheduled
            // (submitted but closure not started) before dropping — the
            // state this regression targets.
            let mut scheduled = false;
            for _ in 0..200 {
                if output_completion_for_test.state() == CaptureState::Scheduled {
                    scheduled = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(
                scheduled,
                "production output must reach Scheduled before drop"
            );
            // Dropped while armed — the owning interview command is dropped/
            // panics while production playback is scheduled-but-not-started.
        }

        // Grace (150ms) expired, the outer wrapper was force-aborted and
        // awaited; the production output closure is STILL gated (never began,
        // never signalled Finished) -> the slot must remain occupied and a
        // second acquire must fail. The outer wrapper abort proves nothing
        // about the physical output worker.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !began.load(Ordering::SeqCst),
            "output closure must still be gated while the slot is held"
        );
        assert!(
            state.is_active().await,
            "slot must stay occupied while production output is unresolved"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "second acquire must fail while production output is unresolved"
        );

        // Open the gate: the output worker begins, observes stop, terminates,
        // and signals Finished. ONLY then may the slot clear.
        gate.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after the production output physically ends"
        );
        assert_eq!(output_completion_for_test.state(), CaptureState::Finished);

        // Reusable.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-1 Test B — production output running during owner cancellation:
    /// the output worker HAS begun (stream running) when the owner is
    /// dropped, the stop flag is set, and the physical output worker stays
    /// alive for a controlled delay before exiting. The slot must remain
    /// occupied until the physical output finishes, and become reusable only
    /// after output_completion == Finished.
    #[tokio::test]
    async fn recording_guard_drop_holds_slot_until_production_tts_output_finishes() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The output worker runs (began) but stays alive a controlled delay
        // after observing stop before it terminates (e.g. stuck in a native
        // device call or a slow teardown).
        let began = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let output_completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            guard.drop_grace = std::time::Duration::from_millis(150);
            let output_completion = guard.output_completion();
            output_completion_for_test = output_completion.clone();
            let worker = {
                let blocking_stop = stop.clone();
                let blocking_began = began.clone();
                let blocking_release = release.clone();
                let output_completion = output_completion.clone();
                tokio::spawn(async move {
                    output_completion.mark_scheduled();
                    let playback = tokio::task::spawn_blocking(move || {
                        // The output worker is RUNNING (stream playing).
                        blocking_began.store(true, Ordering::SeqCst);
                        while !blocking_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        // Stop observed, but the physical output remains
                        // alive for a controlled delay.
                        while !blocking_release.load(Ordering::SeqCst) {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        output_completion.signal();
                    });
                    let _ = playback.await;
                })
            };
            guard.attach_worker(worker);

            // Wait until the output worker is actually RUNNING before the
            // owner is dropped.
            let mut running = false;
            for _ in 0..200 {
                if began.load(Ordering::SeqCst) {
                    running = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert!(running, "production output worker must be running");
            // Owner dropped while the output stream is running.
        }

        // Drop set stop; the running output worker observed it but is still
        // alive (controlled delay). The grace expired, the outer wrapper was
        // aborted/awaited — the slot must STILL be occupied and a second
        // acquire must fail, because the physical output has not finished.
        assert!(stop.load(Ordering::SeqCst), "drop must set the stop flag");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            state.is_active().await,
            "slot must stay occupied while production output is still running"
        );
        let second = acquire_recording(&state, fake_handle()).await;
        assert!(
            second.is_err() && second.unwrap_err().contains("already active"),
            "second acquire must fail while production output is alive"
        );

        // Release the physical output worker -> it terminates -> signals
        // Finished -> the slot clears ONLY then and is reusable.
        release.store(true, Ordering::SeqCst);
        let mut cleared = false;
        for _ in 0..300 {
            if state.slot_is_released().await {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            cleared,
            "slot must clear only after output_completion == Finished"
        );
        assert_eq!(output_completion_for_test.state(), CaptureState::Finished);

        // Reusable only now.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// RC-1 Test C — production TTS exits normally: the output worker runs
    /// to completion and signals Finished; the normal interview path (explicit
    /// slot clear + guard disarm after the worker resolves) proceeds with no
    /// slot leak — the guard's Drop safety net does not re-clear or wait on a
    /// stale completion.
    #[tokio::test]
    async fn production_tts_output_completion_normal_exit_no_slot_leak() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
        });
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        acquire_recording(&state, fake_handle()).await.unwrap();

        let output_completion_for_test;
        {
            let mut guard = RecordingGuard::new(state.clone(), stop.clone());
            let output_completion = guard.output_completion();
            output_completion_for_test = output_completion.clone();
            let worker = {
                let output_completion = output_completion.clone();
                tokio::spawn(async move {
                    // Production playback path: Scheduled before spawn_blocking
                    // (RC-1), then the physical output runs to completion.
                    output_completion.mark_scheduled();
                    let playback = tokio::task::spawn_blocking(move || {
                        // Physical playback completes normally.
                        output_completion.signal();
                    });
                    let _ = playback.await;
                })
            };
            guard.attach_worker(worker);

            // The worker resolves; the output reached Finished. Normal
            // completion path: take the result, clear the slot, disarm.
            guard.wait_worker().await;
            let _ = guard.take_result().await;
            clear_active_recording(&state).await;
            guard.disarm();
            // Dropped disarmed — the safety net must not re-clear or wait.
        }

        assert_eq!(
            output_completion_for_test.state(),
            CaptureState::Finished,
            "normal production TTS exit must reach Finished"
        );
        assert!(
            state.slot_is_released().await,
            "slot must be free after the normal path — no leak"
        );

        // Reusable immediately.
        acquire_recording(&state, fake_handle()).await.unwrap();
        assert!(state.is_active().await);
    }

    /// P2-1: take_result captures the worker's JoinHandle result exactly once.
    #[tokio::test]
    async fn recording_guard_take_result_captures_once() {
        let state = Arc::new(RecordingState {
            slot: Mutex::new(AudioSlotState::Free),
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
            slot: Mutex::new(AudioSlotState::Free),
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
            slot: Mutex::new(AudioSlotState::Free),
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
            slot: Mutex::new(AudioSlotState::Free),
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
        assert!(state.is_active().await);
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
        let outcome = db.insert_round_with_session_update(
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
        );
        assert!(matches!(outcome, db::PersistenceOutcome::Committed(_)));
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

    // ------------------------------------------------------------------
    // P2-1: backend question validation at the Rust boundary
    // ------------------------------------------------------------------

    #[test]
    fn validate_question_rejects_empty() {
        let err = validate_question("").unwrap_err();
        assert!(
            err.contains("cannot be empty"),
            "empty question must be rejected, got: {}",
            err
        );
    }

    #[test]
    fn validate_question_rejects_whitespace_only() {
        let err = validate_question("   \t\n ").unwrap_err();
        assert!(
            err.contains("cannot be empty"),
            "whitespace-only question must be rejected, got: {}",
            err
        );
    }

    #[test]
    fn validate_question_rejects_oversized() {
        let long = "x".repeat(MAX_QUESTION_LEN + 1);
        let err = validate_question(&long).unwrap_err();
        assert!(
            err.contains("too long"),
            "oversized question must be rejected, got: {}",
            err
        );
    }

    #[test]
    fn validate_question_accepts_at_limit() {
        let at_limit = "x".repeat(MAX_QUESTION_LEN);
        assert!(validate_question(&at_limit).is_ok());
    }

    #[test]
    fn validate_question_accepts_and_trims() {
        let trimmed = validate_question("  Tell me about yourself.  ").unwrap();
        assert_eq!(trimmed, "Tell me about yourself.");
    }

    /// P3: the limit is characters, so a multibyte string whose byte length
    /// exceeds the limit but whose character count is within it is accepted.
    #[test]
    fn validate_question_limits_characters_not_bytes() {
        // Each character is 4 UTF-8 bytes; 4000 chars = 16000 bytes, but only
        // 4000 chars — well under the 10_000-character limit.
        let multibyte = "\u{1F600}".repeat(4_000);
        assert!(
            multibyte.len() > MAX_QUESTION_LEN,
            "sanity: bytes exceed limit"
        );
        assert!(validate_question(&multibyte).is_ok());

        // Past the character limit it is rejected regardless of byte width.
        let too_many = "\u{1F600}".repeat(MAX_QUESTION_LEN + 1);
        let err = validate_question(&too_many).unwrap_err();
        assert!(
            err.contains("too long"),
            "character-over-limit must be rejected, got: {}",
            err
        );
    }

    /// P3: ASCII boundary — exactly at the limit passes, one past fails.
    #[test]
    fn validate_question_ascii_boundary() {
        assert!(validate_question(&"x".repeat(MAX_QUESTION_LEN)).is_ok());
        assert!(validate_question(&"x".repeat(MAX_QUESTION_LEN + 1)).is_err());
    }

    // --- Standalone TTS text validation (P3) ---
    // The public standalone `generate_tts` command was removed (RC-G1C); the
    // text validation it used survives here for the retained standalone TTS
    // helpers' contract. The limit is CHARACTERS — `chars().count()` keeps
    // the contract truthful for multibyte text.
    const MAX_TTS_LEN: usize = 10_000;

    fn validate_tts_text(text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("Text cannot be empty".to_string());
        }
        if text.chars().count() > MAX_TTS_LEN {
            return Err(format!("Text too long (max {} characters)", MAX_TTS_LEN));
        }
        Ok(())
    }

    #[test]
    fn validate_tts_text_rejects_empty() {
        assert!(validate_tts_text("").is_err());
        assert!(validate_tts_text("   \t\n ").is_err());
    }

    #[test]
    fn validate_tts_text_rejects_oversized() {
        let err = validate_tts_text(&"x".repeat(MAX_TTS_LEN + 1)).unwrap_err();
        assert!(err.contains("too long"), "got: {}", err);
    }

    #[test]
    fn validate_tts_text_accepts_at_limit() {
        assert!(validate_tts_text(&"x".repeat(MAX_TTS_LEN)).is_ok());
    }

    #[test]
    fn validate_tts_text_limits_characters_not_bytes() {
        // Each character is 4 UTF-8 bytes: 4000 chars = 16000 bytes but only
        // 4000 characters — accepted (byte-length checks would reject it).
        let multibyte = "\u{1F600}".repeat(4_000);
        assert!(multibyte.len() > MAX_TTS_LEN, "sanity: bytes exceed limit");
        assert!(validate_tts_text(&multibyte).is_ok());

        // Past the CHARACTER limit it is rejected regardless of byte width.
        let too_many = "\u{1F600}".repeat(MAX_TTS_LEN + 1);
        let err = validate_tts_text(&too_many).unwrap_err();
        assert!(err.contains("too long"), "got: {}", err);
    }

    // ------------------------------------------------------------------
    // RC-F2: reconciliation journal reaches a resolved state
    // ------------------------------------------------------------------

    const SESSION_UUID: &str = "11111111-1111-1111-1111-111111111111";

    /// Build a temp AppPaths rooted in a tempdir.
    fn temp_paths() -> (tempfile::TempDir, crate::paths::AppPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            dir.path().join("tools"),
            dir.path().join("data"),
        )
        .unwrap();
        // The data dir must exist for Database::open (real startup calls
        // ensure_directories first).
        std::fs::create_dir_all(dir.path().join("data")).unwrap();
        (dir, paths)
    }

    fn journal_path(paths: &crate::paths::AppPaths) -> std::path::PathBuf {
        paths.quarantine_dir().join("reconciliation-needed.log")
    }

    /// RC-F2: an entry whose round is now conclusively committed (matching
    /// sha256) is resolved and dropped from the journal; the durable WAV is
    /// never touched; a second startup emits no stale pending warning.
    #[test]
    fn journal_committed_entry_resolves_and_never_repeats() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();
        db.create_session(SESSION_UUID, "Alice").unwrap();

        let wav = paths.recordings_dir.join(SESSION_UUID).join("round.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"committed-evidence").unwrap();
        let audio_path = wav.to_string_lossy().to_string();
        let sha = "sha-committed";
        let outcome = db.insert_round_with_session_update(
            SESSION_UUID,
            0,
            "Q",
            "A",
            &audio_path,
            sha,
            5000,
            16000,
            1,
            160044,
            false,
        );
        assert!(matches!(outcome, db::PersistenceOutcome::Committed(_)));
        record_reconciliation_needed(&paths, SESSION_UUID, 0, &audio_path, sha, "test");
        assert!(journal_path(&paths).exists());

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages.iter().any(|m| m.contains("resolved (committed)")),
            "committed entry must resolve, got {messages:?}"
        );
        let content = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(
            !content.contains(sha),
            "resolved entry must be dropped from the journal"
        );
        assert_eq!(
            std::fs::read(&wav).unwrap(),
            b"committed-evidence",
            "durable evidence must never be touched by journal resolution"
        );

        // Second startup: no stale pending/repeated resolution for the entry.
        let again = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            !again.iter().any(|m| m.contains(sha)),
            "second startup must not repeat stale warnings, got {again:?}"
        );
    }

    /// RC-F2/RC-F3: an entry whose round has no DB row resolves as not
    /// committed; the unreferenced WAV is moved to a session-scoped
    /// quarantine and the journal entry is dropped.
    #[test]
    fn journal_unreferenced_entry_resolves_and_evidence_is_quarantined() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();

        let wav = paths.recordings_dir.join(SESSION_UUID).join("round.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"orphan-evidence").unwrap();
        let audio_path = wav.to_string_lossy().to_string();
        record_reconciliation_needed(&paths, SESSION_UUID, 0, &audio_path, "sha-orphan", "test");

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("resolved (not committed)")),
            "absent-row entry must resolve, got {messages:?}"
        );
        // Evidence preserved — moved to quarantine/<session>/round.wav.
        let quarantined = paths.quarantine_dir().join(SESSION_UUID).join("round.wav");
        assert!(
            quarantined.exists(),
            "orphan evidence must be quarantined, got {messages:?}"
        );
        assert_eq!(std::fs::read(&quarantined).unwrap(), b"orphan-evidence");
        assert!(!wav.exists());
        // Journal entry resolved away.
        let content = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(!content.contains("sha-orphan"));
    }

    /// RC-F2: an anomalous entry (row exists but sha AND path mismatch) stays
    /// pending; a DB query failure stays pending; a malformed line is
    /// preserved verbatim.
    #[test]
    fn journal_unknown_and_malformed_entries_stay_pending() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();
        db.create_session(SESSION_UUID, "Bob").unwrap();
        // The persisted row has DIFFERENT identity than the journal entry.
        let outcome = db.insert_round_with_session_update(
            SESSION_UUID,
            0,
            "Q",
            "A",
            "/other/path.wav",
            "other-sha",
            5000,
            16000,
            1,
            160044,
            false,
        );
        assert!(matches!(outcome, db::PersistenceOutcome::Committed(_)));
        record_reconciliation_needed(
            &paths,
            SESSION_UUID,
            0,
            "/journal/path.wav",
            "journal-sha",
            "mismatch",
        );
        // Append a malformed line that must survive untouched.
        let malformed = "this is not a valid journal record";
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal_path(&paths))
            .unwrap();
        std::io::Write::write_all(&mut file, format!("\n{malformed}\n").as_bytes()).unwrap();

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("pending reconciliation")),
            "anomalous entry must stay pending, got {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("malformed")),
            "malformed entry must be reported as preserved, got {messages:?}"
        );
        let content = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(
            content.contains("journal-sha"),
            "anomalous entry must remain in the journal"
        );
        assert!(
            content.contains(malformed),
            "malformed entry must be preserved verbatim"
        );
    }

    /// RC-G2: journal identity requires an EXACT match of BOTH sha256 AND
    /// audio path. Path-only, SHA-only, neither, no-row, and DB-failure
    /// classifications are exercised directly on the pure classifier.
    #[test]
    fn journal_identity_requires_exact_sha_and_path() {
        // Exact SHA + exact path -> Committed.
        assert!(matches!(
            classify_journal_identity(Ok(Some(("shaA".into(), "/p.wav".into()))), "/p.wav", "shaA"),
            JournalResolution::Committed
        ));
        // Path-only match (stored sha differs) -> Unknown.
        assert!(matches!(
            classify_journal_identity(Ok(Some(("shaB".into(), "/p.wav".into()))), "/p.wav", "shaA"),
            JournalResolution::Unknown(_)
        ));
        // SHA-only match (stored path differs) -> Unknown.
        assert!(matches!(
            classify_journal_identity(Ok(Some(("shaA".into(), "/q.wav".into()))), "/p.wav", "shaA"),
            JournalResolution::Unknown(_)
        ));
        // Neither matches -> Unknown.
        assert!(matches!(
            classify_journal_identity(Ok(Some(("shaB".into(), "/q.wav".into()))), "/p.wav", "shaA"),
            JournalResolution::Unknown(_)
        ));
        // No row -> NotCommitted.
        assert!(matches!(
            classify_journal_identity(Ok(None), "/p.wav", "shaA"),
            JournalResolution::NotCommitted
        ));
        // DB query failure -> Unknown.
        assert!(matches!(
            classify_journal_identity(Err("simulated DB failure".into()), "/p.wav", "shaA"),
            JournalResolution::Unknown(_)
        ));
    }

    /// RC-G2: a stored row whose PATH matches the journal but whose SHA
    /// differs must stay Unknown/pending — a checksum disagreement is not
    /// "conclusively committed" — and the durable evidence is untouched.
    #[test]
    fn journal_path_only_match_stays_pending() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();
        db.create_session(SESSION_UUID, "PathOnly").unwrap();

        let wav = paths.recordings_dir.join(SESSION_UUID).join("round.wav");
        std::fs::create_dir_all(wav.parent().unwrap()).unwrap();
        std::fs::write(&wav, b"path-match-evidence").unwrap();
        let audio_path = wav.to_string_lossy().to_string();
        let outcome = db.insert_round_with_session_update(
            SESSION_UUID,
            0,
            "Q",
            "A",
            &audio_path,
            "stored-sha",
            5000,
            16000,
            1,
            160044,
            false,
        );
        assert!(matches!(outcome, db::PersistenceOutcome::Committed(_)));
        // Journal entry: same path, DIFFERENT sha -> path-only match.
        record_reconciliation_needed(
            &paths,
            SESSION_UUID,
            0,
            &audio_path,
            "journal-sha",
            "path-only",
        );

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("pending reconciliation")),
            "path-only match must stay pending, got {messages:?}"
        );
        let content = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(
            content.contains("journal-sha"),
            "path-only match must remain in the journal"
        );
        assert_eq!(
            std::fs::read(&wav).unwrap(),
            b"path-match-evidence",
            "durable evidence must never be touched by journal classification"
        );
    }

    /// RC-G2: a stored row whose SHA matches the journal but whose PATH
    /// differs must stay Unknown/pending — path identity disagreement is not
    /// "conclusively committed".
    #[test]
    fn journal_sha_only_match_stays_pending() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();
        db.create_session(SESSION_UUID, "ShaOnly").unwrap();

        let outcome = db.insert_round_with_session_update(
            SESSION_UUID,
            0,
            "Q",
            "A",
            "/db/other.wav",
            "sha-match",
            5000,
            16000,
            1,
            160044,
            false,
        );
        assert!(matches!(outcome, db::PersistenceOutcome::Committed(_)));
        // Journal entry: same sha, DIFFERENT path -> sha-only match.
        record_reconciliation_needed(
            &paths,
            SESSION_UUID,
            0,
            "/journal/path.wav",
            "sha-match",
            "sha-only",
        );

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages
                .iter()
                .any(|m| m.contains("pending reconciliation")),
            "sha-only match must stay pending, got {messages:?}"
        );
        let content = std::fs::read_to_string(journal_path(&paths)).unwrap();
        assert!(
            content.contains("sha-match"),
            "sha-only match must remain in the journal"
        );
    }

    // ------------------------------------------------------------------
    // RC-F3: collision-safe, session-preserving quarantine
    // ------------------------------------------------------------------

    /// RC-F3: same filename under two source sessions yields two preserved
    /// quarantine artifacts; a pre-existing quarantine destination is never
    /// overwritten (alternate name used); a failed quarantine move leaves the
    /// source intact; a successful move preserves bytes exactly.
    #[test]
    fn quarantine_is_session_scoped_and_never_overwrites() {
        let (_dir, paths) = temp_paths();
        let db = db::Database::open(&paths.db_path).unwrap();

        // Session A: unreferenced round.wav; pre-existing quarantine file
        // with the same name must NOT be overwritten.
        let session_a = paths.recordings_dir.join(SESSION_UUID);
        std::fs::create_dir_all(&session_a).unwrap();
        std::fs::write(session_a.join("round.wav"), b"orphan-a").unwrap();
        let existing = paths.quarantine_dir().join(SESSION_UUID).join("round.wav");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        std::fs::write(&existing, b"pre-existing").unwrap();

        // Session B: same filename, different session identity.
        let session_b = paths
            .recordings_dir
            .join("22222222-2222-2222-2222-222222222222");
        std::fs::create_dir_all(&session_b).unwrap();
        std::fs::write(session_b.join("round.wav"), b"orphan-b").unwrap();

        let messages = reconcile_orphaned_evidence(&paths, &db);

        // A's artifact went to the alternate no-overwrite name; the
        // pre-existing file is untouched with its exact bytes.
        let alt = paths
            .quarantine_dir()
            .join(SESSION_UUID)
            .join("round.1.wav");
        assert!(
            alt.exists(),
            "pre-existing quarantine target must not be overwritten, got {messages:?}"
        );
        assert_eq!(std::fs::read(&alt).unwrap(), b"orphan-a");
        assert_eq!(std::fs::read(&existing).unwrap(), b"pre-existing");

        // B's artifact preserved under ITS session directory.
        let b_dest = paths
            .quarantine_dir()
            .join("22222222-2222-2222-2222-222222222222")
            .join("round.wav");
        assert!(b_dest.exists());
        assert_eq!(std::fs::read(&b_dest).unwrap(), b"orphan-b");

        // A failed quarantine move leaves the source intact: make the
        // session's quarantine destination uncreatable by placing a FILE at
        // the session directory path.
        let session_c = paths
            .recordings_dir
            .join("33333333-3333-3333-3333-333333333333");
        std::fs::create_dir_all(&session_c).unwrap();
        std::fs::write(session_c.join("round.wav"), b"orphan-c").unwrap();
        let blocker = paths
            .quarantine_dir()
            .join("33333333-3333-3333-3333-333333333333");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, b"i-am-a-file").unwrap();

        let messages = reconcile_orphaned_evidence(&paths, &db);
        assert!(
            messages.iter().any(|m| m.contains("could not quarantine")
                || m.contains("cannot create quarantine dir")),
            "failed quarantine must be surfaced, got {messages:?}"
        );
        assert_eq!(
            std::fs::read(session_c.join("round.wav")).unwrap(),
            b"orphan-c",
            "failed quarantine must leave the source evidence intact"
        );
    }
}
