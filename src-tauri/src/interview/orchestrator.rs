use crate::audio::capture::{self, CaptureCompletion, CaptureEvent};
use crate::audio::pipe::StderrDrain;
use crate::audio::tts_supervisor::{PiperSupervisor, TtsEvent};
use crate::paths::uuid_to_path;
use sha2::Digest;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Interview phase states
#[derive(Debug, Clone, serde::Serialize)]
pub enum InterviewPhase {
    Idle,
    SpeakingQuestion { question: String },
    Settling { duration_ms: u64 },
    RecordingAnswer,
    Processing,
    Complete,
    Error { message: String },
}

/// Audio metadata for a recorded answer
#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioMetadata {
    pub file_path: String,
    pub sha256: String,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub file_size_bytes: u64,
}

/// A finalized WAV stays PROVISIONAL until the round's DB transaction
/// commits. The guard is created ONLY AFTER capture succeeds (RC-1) — it is
/// never armed for a path this invocation did not create — stays armed
/// across checksum/transcription, and is returned to the persistence-owning
/// layer (lib.rs), which commits (disarms) it only after the DB transaction
/// succeeds. Any pre-commit drop — stop, timeout, checksum failure,
/// transcription failure, DB failure — deletes the WAV and its partial temp
/// file. Once committed, the persisted evidence is never deleted
/// automatically. `owns_wav` is defense-in-depth: a guard created for a path
/// that pre-existed (e.g. a colliding round_id that `record_to_wav`
/// rejected) must never delete that pre-existing committed evidence.
#[derive(Debug)]
pub struct UnpersistedAudio {
    wav_path: std::path::PathBuf,
    owns_wav: bool,
    committed: bool,
    /// Explicit controlled cleanup succeeded — Drop must not retry.
    cleaned: bool,
}

impl UnpersistedAudio {
    /// Create the guard for a WAV THIS invocation just created. Must be
    /// called only after a successful `record_to_wav` (RC-1): the file is
    /// owned by this invocation and is removed if the round never commits.
    fn new(wav_path: std::path::PathBuf) -> Self {
        Self {
            wav_path,
            owns_wav: true,
            committed: false,
            cleaned: false,
        }
    }

    /// Mark the WAV as durably persisted (DB commit succeeded). Deletion on
    /// drop is then suppressed.
    pub fn commit(&mut self) {
        self.committed = true;
    }

    /// Explicit CONTROLLED cleanup (RC-1C) for the conclusive-failure path
    /// (persistence conclusively absent). Returns Err with the details when
    /// any owned artifact could not be removed — the guard stays armed, so
    /// Drop retries best-effort, and startup reconciliation catches whatever
    /// remains. Success is never reported while a known artifact remains.
    pub fn cleanup(&mut self) -> Result<(), String> {
        let mut failures = Vec::new();
        for path in [
            self.wav_path.clone(),
            self.wav_path.with_extension("wav.tmp"),
        ] {
            if !path.exists() {
                continue;
            }
            let outcome = crate::cleanup::remove_owned(&path);
            if outcome.failed() {
                failures.push(crate::cleanup::describe(&outcome, &path));
            }
        }
        if failures.is_empty() {
            self.cleaned = true;
            Ok(())
        } else {
            Err(format!(
                "candidate evidence cleanup failed: {}",
                failures.join("; ")
            ))
        }
    }

    /// Preserve the WAV in place (ambiguous persistence outcome — RC-4):
    /// neither committed nor deleted. Drop is disarmed so the evidence
    /// survives for startup reconciliation.
    pub fn preserve(&mut self) {
        self.committed = true;
        self.cleaned = true;
    }
}

impl Drop for UnpersistedAudio {
    fn drop(&mut self) {
        if !self.committed && !self.cleaned && self.owns_wav {
            let _ = std::fs::remove_file(&self.wav_path);
            // Partial recordings land in a temp file next to the final path.
            let _ = std::fs::remove_file(self.wav_path.with_extension("wav.tmp"));
        }
    }
}

/// Compute SHA-256 hash of a file
fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Post-capture transition (RC-5): given a SUCCESSFUL capture result, arm the
/// provisional evidence guard, then check the stop flag. This exactly models
/// the real orchestration ordering:
///
///   capture succeeds -> guard armed -> stop flag checked
///   -> if stopped: bail cancels the round and the dropped guard removes the WAV
///   -> if not stopped: Ok(guard) for the caller to commit after persistence
///
/// A stopped round never leaves an orphan final WAV (the guard removes it on
/// drop), and a pre-existing committed WAV is never touched (the guard was
/// created for a path `record_to_wav` rejected before recording — the
/// pre-existing collision path never reaches this function).
pub(crate) fn handoff_captured_evidence(
    wav_path: std::path::PathBuf,
    stop_flag: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<UnpersistedAudio> {
    let provisional = UnpersistedAudio::new(wav_path);
    if stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during recording");
    }
    Ok(provisional)
}

/// Orchestrate a half-duplex interview round.
/// Returns (audio_metadata, transcription_text, unpersisted_audio) on
/// success. The `UnpersistedAudio` guard stays ARMED: the caller must commit
/// it only after the round's DB transaction commits. Full-round timeout and
/// grace-period enforcement live in lib.rs.
/// `completion` is the microphone-capture lifecycle signal; `output_completion`
/// is the PRODUCTION TTS playback lifecycle signal (RC-1) — forwarded to
/// `PiperSupervisor::speak` so the shared audio slot stays occupied until the
/// physical question playback has fully exited, not just until the outer
/// worker returns.
#[allow(clippy::too_many_arguments)]
pub async fn run_interview_round(
    question: &str,
    paths: &crate::paths::AppPaths,
    session_id: Uuid,
    round_id: Uuid,
    event_tx: mpsc::Sender<CaptureEvent>,
    tts_event_tx: mpsc::Sender<TtsEvent>,
    stop_flag: Arc<AtomicBool>,
    phase_tx: Option<mpsc::Sender<InterviewPhase>>,
    completion: CaptureCompletion,
    output_completion: CaptureCompletion,
    process_completion: crate::audio::pipe::ProcessCompletion,
) -> anyhow::Result<(AudioMetadata, String, UnpersistedAudio)> {
    let piper = PiperSupervisor::new(paths)?;

    // Phase 1: Speak the question
    let _ = tts_event_tx.try_send(TtsEvent::Speaking {
        text: question.to_string(),
    });
    let _ = phase_tx.as_ref().map(|tx| {
        tx.try_send(InterviewPhase::SpeakingQuestion {
            question: question.to_string(),
        })
    });

    piper
        .speak(
            question,
            tts_event_tx.clone(),
            stop_flag.clone(),
            output_completion,
            process_completion.clone(),
        )
        .await?;

    if stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during TTS");
    }

    // Phase 2: Settling period (1.5s) - let speaker output settle before recording
    let settle_ms = 1500u64;
    let _ = phase_tx.as_ref().map(|tx| {
        tx.try_send(InterviewPhase::Settling {
            duration_ms: settle_ms,
        })
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(settle_ms)).await;

    if stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during settle");
    }

    // Phase 3: Record the answer — isolated to recordings/<session_id>/<round_id>.wav
    let _ = phase_tx
        .as_ref()
        .map(|tx| tx.try_send(InterviewPhase::RecordingAnswer));

    let session_dir = paths.session_recordings_dir(session_id);
    std::fs::create_dir_all(&session_dir)?;
    let wav_path = session_dir.join(format!("{}.wav", uuid_to_path(&round_id)));

    let record_event_tx = event_tx.clone();

    // Use a separate stop flag for recording (auto-stop after 60s or silence)
    let record_auto_stop = Arc::new(AtomicBool::new(false));
    let record_auto_stop_clone = record_auto_stop.clone();

    // Auto-stop timer (max 60 seconds per answer)
    let auto_stop_handle = tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        record_auto_stop_clone.store(true, Ordering::SeqCst);
    });

    // Also monitor the main stop flag
    let main_stop = stop_flag.clone();
    let auto_stop_for_main = record_auto_stop.clone();
    let main_monitor = tokio::spawn(async move {
        loop {
            if main_stop.load(Ordering::SeqCst) {
                auto_stop_for_main.store(true, Ordering::SeqCst);
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    });

    let record_result = capture::record_to_wav(
        wav_path.clone(),
        16000, // 16kHz for whisper compatibility
        1,     // mono
        record_event_tx.clone(),
        record_auto_stop,
        completion,
        true, // RC-1: interview rounds RETAIN the final WAV (provisional evidence)
    )
    .await;

    auto_stop_handle.abort();
    main_monitor.abort();

    let record_result = match record_result {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.try_send(CaptureEvent::Error {
                message: format!("Recording failed: {}", e),
            });
            return Err(e);
        }
    };

    // RC-5: the post-capture transition is extracted as a testable seam.
    // Must run immediately after capture SUCCEEDS and BEFORE any later work:
    // arm the provisional guard, then check the stop flag. A stopped round
    // drops the armed guard on bail, removing only this invocation's WAV.
    let provisional = handoff_captured_evidence(record_result.file_path.clone(), &stop_flag)?;

    // Phase 4: Compute audio metadata and checksum from RecordResult
    let sha256 = sha256_file(&record_result.file_path)?;

    let metadata = AudioMetadata {
        file_path: record_result.file_path.to_string_lossy().to_string(),
        sha256,
        duration_ms: record_result.duration_ms,
        sample_rate: 16000,
        channels: 1,
        file_size_bytes: record_result.file_size_bytes,
    };

    // Phase 5: Transcribe with whisper — temp file isolated to temp/<session_id>/<round_id>.txt
    let _ = phase_tx
        .as_ref()
        .map(|tx| tx.try_send(InterviewPhase::Processing));

    let transcription = transcribe_wav(
        paths,
        &wav_path,
        session_id,
        round_id,
        stop_flag,
        process_completion,
    )
    .await?;

    // NOTE: "complete" is deliberately NOT emitted here. The frontend must
    // only see "complete" after the round is durably persisted, which happens
    // in the persistence-owning layer (lib.rs) after the DB COMMIT.

    // Round succeeded end-to-end, but the WAV is still provisional: hand the
    // armed guard to the caller so it can commit only after persistence.
    Ok((metadata, transcription, provisional))
}

/// Process-level timeout for whisper.cpp transcription.
/// Spawns the child with `kill_on_drop(true)`, coordinates via `tokio::select!`,
/// kills on timeout or cancellation, and cleans up temp files.
const WHISPER_TIMEOUT_SECS: u64 = 120;

/// Poll the stop flag every 50ms. Returns when the flag is set.
/// Used by both Whisper and Piper cancellation branches to ensure reliable
/// wake-up even when the select guard is not re-evaluated on flag change.
pub async fn wait_for_stop(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

/// Invocation-owned Whisper transcript temp file (RC-3). The path is
/// invocation-unique (`<round>.<invocation>.txt`), so a stale transcript from
/// a previous crashed invocation or a deterministic legacy `<round>.txt` can
/// never be read or deleted by this invocation. The guard is armed from
/// creation and disarmed only after a CONTROLLED deletion is confirmed; Drop
/// stays as a best-effort fallback and startup reconciliation sweeps any
/// `.txt` under the session temp dir.
struct TranscriptTempGuard {
    path: std::path::PathBuf,
    armed: bool,
}

impl TranscriptTempGuard {
    fn new(path: std::path::PathBuf) -> Self {
        Self { path, armed: true }
    }

    /// Controlled deletion (RC-1D): failure is observable, never swallowed.
    /// On success the guard is disarmed; on failure it stays armed so Drop
    /// retries and the leftover is reconcilable at startup.
    fn cleanup(&mut self) -> crate::cleanup::CleanupOutcome {
        let outcome = crate::cleanup::remove_owned(&self.path);
        if outcome.succeeded() {
            self.armed = false;
        }
        outcome
    }
}

impl Drop for TranscriptTempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Read Whisper's transcript — ONLY this invocation's unique temp file. The
/// caller owns cleanup via `TranscriptTempGuard`.
fn read_transcript(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// RC-3: build the invocation-unique Whisper output stem.
/// `temp/<session>/<round>.<invocation>.txt` — the legacy deterministic
/// `<round>.txt` is never produced or read by this code.
fn whisper_output_stem(round_id: Uuid, invocation: Uuid) -> String {
    format!("{}.{}", uuid_to_path(&round_id), uuid_to_path(&invocation))
}

/// Transcribe a WAV file using whisper.cpp. The output stem is
/// INVOCATION-UNIQUE (RC-3): `temp/<session>/<round>.<invocation>` — never a
/// deterministic shared `temp/<session>/<round>.txt` — so a retry, a stale
/// file from a crashed run, or a concurrently-created transcript can never be
/// read or deleted by a different invocation. Only the transcription text is
/// persisted; the temp file needs no stable name.
async fn transcribe_wav(
    paths: &crate::paths::AppPaths,
    wav_path: &std::path::Path,
    session_id: Uuid,
    round_id: Uuid,
    stop_flag: Arc<AtomicBool>,
    process_completion: crate::audio::pipe::ProcessCompletion,
) -> anyhow::Result<String> {
    use std::process::Stdio;

    let tools = crate::paths::resolve_tools(&paths.tool_dir);
    let whisper_bin = tools
        .whisper_bin
        .ok_or_else(|| anyhow::anyhow!("Whisper binary not found"))?;
    let model_path = tools
        .whisper_model
        .ok_or_else(|| anyhow::anyhow!("Whisper model not found"))?;

    if !whisper_bin.exists() {
        anyhow::bail!("Whisper binary not found at {}", whisper_bin.display());
    }
    if !model_path.exists() {
        anyhow::bail!("Whisper model not found at {}", model_path.display());
    }

    let output_dir = paths.temp_dir.join(session_id.hyphenated().to_string());
    std::fs::create_dir_all(&output_dir)?;
    // RC-3: unique stem per invocation — `<round>.<invocation>`. The legacy
    // deterministic `<round>.txt` is never produced or read by this code.
    let invocation = uuid::Uuid::new_v4();
    let stem = whisper_output_stem(round_id, invocation);
    let txt_path = output_dir.join(format!("{}.txt", stem));
    // Owns THIS invocation's transcript from here: armed until deletion is
    // confirmed (RC-1D/RC-3).
    let mut transcript_guard = TranscriptTempGuard::new(txt_path.clone());

    // RC-2B: mark Running BEFORE spawn (no await in between) so a force-abort
    // can never resolve ownership while the Whisper child may be alive.
    process_completion.mark_running();
    let child = match tokio::process::Command::new(&whisper_bin)
        .arg("--model")
        .arg(&model_path)
        .arg("--file")
        .arg(wav_path)
        .arg("--language")
        .arg("en")
        .arg("-otxt")
        .arg("-of")
        .arg(output_dir.join(&stem))
        // stdout is not used for the transcription result (it goes to -otxt),
        // so point it at null; stderr is drained concurrently so whisper can
        // never block on a full pipe while we wait for it to exit.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // Spawn failed — nothing was ever alive to reap.
            process_completion.signal_reaped();
            return Err(anyhow::anyhow!("Failed to start Whisper: {}", e));
        }
    };
    // The child's kill+wait lifecycle is RAII-owned (RC-2B): on every exit
    // path — including force-abort — the child is reaped before the
    // completion signals Reaped.
    let mut proc_guard =
        crate::audio::pipe::ChildProcessGuard::new(child, process_completion.clone());

    let stderr_drain = proc_guard.child_mut().stderr.take().map(StderrDrain::start);

    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(WHISPER_TIMEOUT_SECS);

    let wait_result = tokio::select! {
        biased;

        result = proc_guard.child_mut().wait() => result,
        _ = wait_for_stop(stop_flag) => {
            proc_guard.terminate().await;
            anyhow::bail!("Whisper cancelled — process killed");
        }
        _ = tokio::time::sleep_until(deadline) => {
            proc_guard.terminate().await;
            anyhow::bail!(
                "Whisper timed out after {}s — process killed",
                WHISPER_TIMEOUT_SECS
            );
        }
    };

    let status = match wait_result {
        Ok(status) => status,
        Err(e) => {
            // Child state is uncertain after a wait error — terminate and
            // reap explicitly before propagating (P2-2); kill_on_drop stays
            // only as defense-in-depth.
            proc_guard.terminate().await;
            anyhow::bail!("Whisper wait error: {}", e);
        }
    };
    // The child exited and was waited — conclusively reaped (RC-2B).
    proc_guard.mark_reaped();
    if !status.success() {
        // Non-zero exit — surface stderr. The transcript guard cleans this
        // invocation's temp on drop.
        let stderr_text = match &stderr_drain {
            Some(d) => d.text().await,
            None => String::new(),
        };
        anyhow::bail!(
            "Whisper failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_text.trim()
        );
    }

    // Read ONLY this invocation's file. On success OR failure (e.g. invalid
    // UTF-8) the temp is cleaned up with controlled semantics (RC-1D); a
    // cleanup failure is observable and reconcilable, never swallowed.
    let read_result = read_transcript(&txt_path);
    match read_result {
        Ok(text) => {
            let outcome = transcript_guard.cleanup();
            if outcome.failed() {
                eprintln!(
                    "[transcribe] transcript cleanup failed: {}",
                    crate::cleanup::describe(&outcome, &txt_path)
                );
            }
            Ok(text.trim().to_string())
        }
        Err(e) => {
            let outcome = transcript_guard.cleanup();
            if outcome.failed() {
                eprintln!(
                    "[transcribe] transcript cleanup failed: {}",
                    crate::cleanup::describe(&outcome, &txt_path)
                );
            }
            Err(anyhow::anyhow!("Failed to read transcript: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RC-3: two invocations for the same logical round get DIFFERENT output
    /// stems — a retry can never share a transcript path with a previous
    /// invocation, and the legacy deterministic `<round>.txt` is never the
    /// active output.
    #[test]
    fn whisper_output_stem_is_invocation_unique() {
        let round_id = uuid::Uuid::new_v4();
        let invocation_a = uuid::Uuid::new_v4();
        let invocation_b = uuid::Uuid::new_v4();

        let stem_a = whisper_output_stem(round_id, invocation_a);
        let stem_b = whisper_output_stem(round_id, invocation_b);
        assert_ne!(stem_a, stem_b, "invocations must not share a stem");

        // The active path is `<round>.<invocation>.txt` — never the legacy
        // deterministic `<round>.txt`.
        assert_ne!(
            format!("{}.txt", stem_a),
            format!("{}.txt", uuid_to_path(&round_id)),
            "the deterministic legacy path must never be the active output"
        );
        assert!(
            stem_a.starts_with(&format!("{}.", uuid_to_path(&round_id))),
            "the stem must carry the round identity as a prefix"
        );
    }

    /// RC-3: a stale legacy deterministic `<round>.txt` (from a previous
    /// crashed invocation) is never the path a new invocation reads — the
    /// new invocation reads only its own unique file.
    #[test]
    fn stale_legacy_transcript_cannot_be_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let round_id = uuid::Uuid::new_v4();
        let invocation = uuid::Uuid::new_v4();

        // Pre-create the legacy deterministic path with stale content.
        let legacy = dir.path().join(format!("{}.txt", uuid_to_path(&round_id)));
        std::fs::write(&legacy, b"STALE TRANSCRIPT THAT MUST NEVER BE READ").unwrap();

        // The invocation reads its own unique path — the stale legacy file is
        // untouched and never read.
        let own = dir
            .path()
            .join(format!("{}.txt", whisper_output_stem(round_id, invocation)));
        assert_ne!(legacy, own);
        std::fs::write(&own, b"fresh").unwrap();
        assert_eq!(read_transcript(&own).unwrap(), "fresh");
        // The stale file is still exactly as it was — never read, never
        // deleted by this invocation.
        assert_eq!(
            std::fs::read(&legacy).unwrap(),
            b"STALE TRANSCRIPT THAT MUST NEVER BE READ"
        );
    }

    /// RC-1D/RC-3: an invalid-UTF-8 transcript read fails AND the owned temp
    /// is still cleaned up with controlled semantics.
    #[test]
    fn transcript_read_failure_still_cleans_up_owned_temp() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("round.inv.txt");
        std::fs::write(&transcript, [0xff, 0xfe, 0xfd]).unwrap();

        let result = read_transcript(&transcript);
        assert!(result.is_err(), "invalid UTF-8 must fail transcription");

        let mut guard = TranscriptTempGuard::new(transcript.clone());
        let outcome = guard.cleanup();
        assert!(outcome.succeeded(), "controlled cleanup must succeed");
        assert!(
            !transcript.exists(),
            "failed transcript reads must not leave temp output behind"
        );
    }

    /// RC-1D: a TranscriptTempGuard is disarmed only after a CONFIRMED
    /// deletion — a failed removal keeps it armed so Drop retries.
    #[test]
    fn transcript_temp_guard_disarms_only_after_confirmed_removal() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("round.inv.txt");
        std::fs::write(&transcript, b"x").unwrap();

        // Successful removal -> disarmed -> Drop does nothing.
        {
            let mut guard = TranscriptTempGuard::new(transcript.clone());
            assert!(guard.cleanup().succeeded());
            drop(guard);
        }
        assert!(!transcript.exists());

        // Unremovable target (a directory) -> cleanup fails -> guard stays
        // armed -> Drop retries (still fails, but the failure path exists and
        // the leftover is reconcilable at startup).
        let target = dir.path().join("a-directory");
        std::fs::create_dir_all(&target).unwrap();
        {
            let mut guard = TranscriptTempGuard::new(target.clone());
            let outcome = guard.cleanup();
            assert!(outcome.failed(), "unremovable target must report failure");
            drop(guard); // Drop retries best-effort
        }
    }

    /// A WAV left uncommitted (round failed before persistence) must be
    /// deleted when the guard drops, including any partial temp file.
    #[test]
    fn unpersisted_audio_deletes_wav_and_tmp_on_drop_without_commit() {
        let dir = std::env::temp_dir().join("unpersisted_audio_delete_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        let tmp = dir.join("round.wav.tmp");
        std::fs::write(&wav, b"audio").unwrap();
        std::fs::write(&tmp, b"partial").unwrap();

        {
            let _unpersisted = UnpersistedAudio::new(wav.clone());
            // dropped without commit — simulates a failed round
        }

        assert!(!wav.exists(), "uncommitted WAV must be deleted");
        assert!(!tmp.exists(), "partial temp WAV must be deleted");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-1C: explicit controlled cleanup removes the WAV and temp, and its
    /// result is observable — success is never claimed while an artifact may
    /// remain; a failed removal keeps the guard armed so Drop retries.
    #[test]
    fn unpersisted_audio_explicit_cleanup_result_is_observable() {
        let dir = std::env::temp_dir().join("unpersisted_audio_cleanup_result_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        let tmp = dir.join("round.wav.tmp");
        std::fs::write(&wav, b"audio").unwrap();
        std::fs::write(&tmp, b"partial").unwrap();

        {
            let mut unpersisted = UnpersistedAudio::new(wav.clone());
            assert!(unpersisted.cleanup().is_ok(), "cleanup must succeed");
            drop(unpersisted);
        }
        assert!(!wav.exists(), "explicit cleanup must remove the WAV");
        assert!(!tmp.exists(), "explicit cleanup must remove the temp");

        // Failure path: an unremovable target (a directory) reports Err; the
        // guard stays armed and Drop retries best-effort.
        let target = dir.join("a-directory");
        std::fs::create_dir_all(&target).unwrap();
        {
            let mut unpersisted = UnpersistedAudio::new(target.clone());
            assert!(
                unpersisted.cleanup().is_err(),
                "unremovable target must report a cleanup failure"
            );
            drop(unpersisted); // Drop retries best-effort (still fails)
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-4: preserve() disarms deletion — ambiguous-persistence evidence is
    /// never deleted based on an unverified assumption.
    #[test]
    fn unpersisted_audio_preserve_never_deletes() {
        let dir = std::env::temp_dir().join("unpersisted_audio_preserve_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        std::fs::write(&wav, b"audio").unwrap();

        {
            let mut unpersisted = UnpersistedAudio::new(wav.clone());
            unpersisted.preserve();
        }
        assert!(
            wav.exists(),
            "preserved evidence must never be deleted, even on drop"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A committed WAV (round persisted) must never be deleted automatically.
    #[test]
    fn unpersisted_audio_keeps_wav_after_commit() {
        let dir = std::env::temp_dir().join("unpersisted_audio_commit_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        std::fs::write(&wav, b"audio").unwrap();

        {
            let mut unpersisted = UnpersistedAudio::new(wav.clone());
            unpersisted.commit();
        }

        assert!(wav.exists(), "committed WAV must be retained");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-1 stop-round evidence lifecycle (Test A): a capture that SUCCEEDS
    /// (the temp was finalized and renamed to the final WAV) followed by a
    /// post-capture stop must NOT leave an orphan final WAV. The orchestrator
    /// arms the outer evidence guard immediately after capture success and
    /// BEFORE the stop check, so a stopped round drops the armed guard,
    /// which removes only the newly-created WAV. This mirrors the
    /// orchestrator ordering exactly.
    #[test]
    fn stopped_round_removes_newly_created_final_wav() {
        let dir = std::env::temp_dir().join("unpersisted_audio_stop_round_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        // Capture succeeded: the temp was renamed to the final WAV.
        std::fs::write(&wav, b"recorded-audio").unwrap();

        // The orchestrator arms the evidence guard immediately after capture
        // success, BEFORE the post-capture stop check (RC-1).
        let provisional = UnpersistedAudio::new(wav.clone());
        // Post-capture stop -> bail -> guard drops -> WAV removed.
        drop(provisional);

        assert!(
            !wav.exists(),
            "stopped round must not leave an orphan final WAV"
        );
        assert!(
            !wav.with_extension("wav.tmp").exists(),
            "no stale temp may remain after a stopped round"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A guard that is handed to the caller stays armed: dropping it there
    /// (pre-commit failure in the persistence layer) still deletes the WAV.
    #[test]
    fn unpersisted_audio_armed_after_handoff_deletes_wav() {
        let dir = std::env::temp_dir().join("unpersisted_audio_handoff_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        std::fs::write(&wav, b"audio").unwrap();

        // Worker success hands the guard to the caller WITHOUT committing.
        let guard = UnpersistedAudio::new(wav.clone());
        // Persistence fails -> guard dropped pre-commit -> WAV removed.
        drop(guard);
        assert!(!wav.exists(), "pre-commit failure must delete the WAV");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-5 stop-round orchestration regression: successful capture result →
    /// guard creation → stop flag true → bail removes the newly-created WAV.
    /// This exercises the exact ordering the real orchestrator uses.
    #[test]
    fn handoff_captured_evidence_stop_removes_wav() {
        let dir = std::env::temp_dir().join("handoff_captured_evidence_stop_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        std::fs::write(&wav, b"captured-audio").unwrap();

        let stop = std::sync::atomic::AtomicBool::new(true);
        let result = handoff_captured_evidence(wav.clone(), &stop);

        assert!(result.is_err(), "stop flag true must produce Err");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("stopped during recording"),
            "error must describe the stop, got: {}",
            err
        );
        assert!(
            !wav.exists(),
            "newly-created WAV must be removed by guard drop on stop"
        );
        assert!(
            !wav.with_extension("wav.tmp").exists(),
            "no stale temp may remain after a stopped round"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RC-5 positive path: stop flag false → Ok(guard) returned. The guard
    /// is armed (no commit), so dropping it removes the WAV.
    #[test]
    fn handoff_captured_evidence_ok_guard_drops_wav() {
        let dir = std::env::temp_dir().join("handoff_captured_evidence_ok_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let wav = dir.join("round.wav");
        std::fs::write(&wav, b"captured-audio").unwrap();

        let stop = std::sync::atomic::AtomicBool::new(false);
        let result = handoff_captured_evidence(wav.clone(), &stop);

        assert!(result.is_ok(), "stop flag false must produce Ok");
        assert!(
            wav.exists(),
            "WAV must survive the handoff before guard drop"
        );

        // Drop the guard without commit → WAV removed (provisional still armed).
        drop(result.unwrap());
        assert!(!wav.exists(), "uncommitted guard drop must remove the WAV");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
