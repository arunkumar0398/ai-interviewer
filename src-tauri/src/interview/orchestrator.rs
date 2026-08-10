use crate::audio::capture::{self, CaptureCompletion, CaptureEvent};
use crate::audio::pipe::{terminate_child, StderrDrain};
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
        }
    }

    /// Mark the WAV as durably persisted (DB commit succeeded). Deletion on
    /// drop is then suppressed.
    pub fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for UnpersistedAudio {
    fn drop(&mut self) {
        if !self.committed && self.owns_wav {
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

    let transcription = transcribe_wav(paths, &wav_path, session_id, round_id, stop_flag).await?;

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

/// Read Whisper's transcript and remove the per-round temp output before
/// propagating either success or failure (including invalid UTF-8).
fn read_transcript_and_remove(path: &Path) -> std::io::Result<String> {
    let result = std::fs::read_to_string(path);
    let _ = std::fs::remove_file(path);
    result
}

/// Transcribe a WAV file using whisper.cpp — temp output isolated to temp/<session_id>/<round_id>.txt
async fn transcribe_wav(
    paths: &crate::paths::AppPaths,
    wav_path: &std::path::Path,
    session_id: Uuid,
    round_id: Uuid,
    stop_flag: Arc<AtomicBool>,
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
    let stem = uuid_to_path(&round_id);
    let txt_path = output_dir.join(format!("{}.txt", stem));

    let mut child = tokio::process::Command::new(&whisper_bin)
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
        .spawn()?;

    let stderr_drain = child.stderr.take().map(StderrDrain::start);

    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(WHISPER_TIMEOUT_SECS);

    let wait_result = tokio::select! {
        biased;

        result = child.wait() => result,
        _ = wait_for_stop(stop_flag) => {
            terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&txt_path);
            anyhow::bail!("Whisper cancelled — process killed, temp cleaned");
        }
        _ = tokio::time::sleep_until(deadline) => {
            terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&txt_path);
            anyhow::bail!(
                "Whisper timed out after {}s — process killed, temp cleaned",
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
            terminate_child(&mut child).await;
            let _ = std::fs::remove_file(&txt_path);
            anyhow::bail!("Whisper wait error: {}", e);
        }
    };
    if !status.success() {
        // Non-zero exit — remove the partial transcript and surface stderr.
        let _ = std::fs::remove_file(&txt_path);
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

    let text = read_transcript_and_remove(&txt_path)?;

    Ok(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_read_failure_still_removes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("round.txt");
        std::fs::write(&transcript, [0xff, 0xfe, 0xfd]).unwrap();

        let result = read_transcript_and_remove(&transcript);

        assert!(result.is_err(), "invalid UTF-8 must fail transcription");
        assert!(
            !transcript.exists(),
            "failed transcript reads must not leave temp output behind"
        );
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
