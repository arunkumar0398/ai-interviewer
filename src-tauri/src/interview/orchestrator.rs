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
pub async fn run_interview_round(
    question: &str,
    paths: &crate::paths::AppPaths,
    session_id: Uuid,
    round_id: Uuid,
    event_tx: mpsc::Sender<CaptureEvent>,
    tts_event_tx: mpsc::Sender<TtsEvent>,
    stop_flag: Arc<AtomicBool>,
) -> anyhow::Result<(AudioMetadata, String)> {
    let piper = PiperSupervisor::new(paths)?;

    // Phase 1: Speak the question
    let _ = tts_event_tx.try_send(TtsEvent::Speaking {
        text: question.to_string(),
    });

    piper
        .speak(question, tts_event_tx.clone(), stop_flag.clone())
        .await?;

    if stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during TTS");
    }

    // Phase 2: Settling period (1.5s) - let speaker output settle before recording
    let settle_ms = 1500u64;
    tokio::time::sleep(tokio::time::Duration::from_millis(settle_ms)).await;

    if stop_flag.load(Ordering::SeqCst) {
        anyhow::bail!("Interview stopped during settle");
    }

    // Phase 3: Record the answer — isolated to recordings/<session_id>/<round_id>.wav
    let session_dir = paths.session_recordings_dir(session_id);
    std::fs::create_dir_all(&session_dir)?;
    let wav_path = session_dir.join(format!("{}.wav", uuid_to_path(&round_id)));

    let _record_stop = stop_flag.clone();
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
    let transcription = transcribe_wav(paths, &wav_path, session_id, round_id).await?;

    Ok((metadata, transcription))
}

/// Transcribe a WAV file using whisper.cpp — temp output isolated to temp/<session_id>/<round_id>.txt
async fn transcribe_wav(
    paths: &crate::paths::AppPaths,
    wav_path: &std::path::Path,
    session_id: Uuid,
    round_id: Uuid,
) -> anyhow::Result<String> {
    let whisper_bin = crate::paths::resolve_whisper_path(&paths.tool_dir)
        .ok_or_else(|| anyhow::anyhow!("Whisper binary not found"))?;
    let model_path = crate::paths::resolve_whisper_model_path(&paths.tool_dir)
        .ok_or_else(|| anyhow::anyhow!("Whisper model not found"))?;

    if !whisper_bin.exists() {
        anyhow::bail!("Whisper binary not found at {}", whisper_bin.display());
    }
    if !model_path.exists() {
        anyhow::bail!("Whisper model not found at {}", model_path.display());
    }

    // Clone only what's needed for the blocking task
    let whisper_bin = whisper_bin.clone();
    let model_path = model_path.clone();
    let wav_path = wav_path.to_path_buf();
    let output_dir = paths.temp_dir.join(session_id.hyphenated().to_string());
    std::fs::create_dir_all(&output_dir)?;
    let stem_clone = uuid_to_path(&round_id);

    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new(&whisper_bin)
            .arg("--model")
            .arg(&model_path)
            .arg("--file")
            .arg(&wav_path)
            .arg("--language")
            .arg("en")
            .arg("-otxt")
            .arg("-of")
            .arg(output_dir.join(&stem_clone))
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Whisper failed: {}", stderr);
        }

        let txt_path = output_dir.join(format!("{}.txt", stem_clone));
        let text = std::fs::read_to_string(&txt_path)?;
        let _ = std::fs::remove_file(&txt_path); // cleanup

        Ok(text.trim().to_string())
    })
    .await?
}
