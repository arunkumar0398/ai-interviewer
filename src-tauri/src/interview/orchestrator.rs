use crate::audio::capture::{self, CaptureEvent};
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

/// Orchestrate a half-duplex interview round.
/// Returns (audio_metadata, transcription_text) on success.
/// Full-round timeout and grace-period enforcement live in lib.rs.
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
) -> anyhow::Result<(AudioMetadata, String)> {
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
        .speak(question, tts_event_tx.clone(), stop_flag.clone())
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

    if stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during recording");
    }

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

    let _ = phase_tx
        .as_ref()
        .map(|tx| tx.try_send(InterviewPhase::Complete));

    Ok((metadata, transcription))
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

/// Transcribe a WAV file using whisper.cpp — temp output isolated to temp/<session_id>/<round_id>.txt
async fn transcribe_wav(
    paths: &crate::paths::AppPaths,
    wav_path: &std::path::Path,
    session_id: Uuid,
    round_id: Uuid,
    stop_flag: Arc<AtomicBool>,
) -> anyhow::Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;

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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(WHISPER_TIMEOUT_SECS);

    let wait_result = tokio::select! {
        biased;

        result = child.wait() => result,
        _ = wait_for_stop(stop_flag) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&txt_path);
            anyhow::bail!("Whisper cancelled — process killed, temp cleaned");
        }
        _ = tokio::time::sleep_until(deadline) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = std::fs::remove_file(&txt_path);
            anyhow::bail!(
                "Whisper timed out after {}s — process killed, temp cleaned",
                WHISPER_TIMEOUT_SECS
            );
        }
    };

    let status = wait_result?;
    if !status.success() {
        let mut stderr = child.stderr.take().unwrap();
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf).await;
        anyhow::bail!(
            "Whisper failed (exit {}): {}",
            status.code().unwrap_or(-1),
            buf
        );
    }

    let text = std::fs::read_to_string(&txt_path)?;
    let _ = std::fs::remove_file(&txt_path);

    Ok(text.trim().to_string())
}
