use crate::audio::capture::{
    record_test_clip, validate_production_playback_stream, CaptureCompletion, CaptureEvent,
};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Device check result
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceCheckResult {
    pub mic_available: bool,
    pub mic_name: Option<String>,
    pub speaker_available: bool,
    pub speaker_name: Option<String>,
    pub mic_test_ok: bool,
    pub errors: Vec<String>,
}

/// Verify microphone and speaker devices are available and functional.
/// Records a short test clip to verify mic actually captures audio.
/// `stop_flag` is the owning command's cancellation signal (P2-2): it is
/// propagated into the test-clip capture loop so an outer cancellation
/// terminates the capture promptly instead of waiting for the deadline.
///
/// RC-2 (native-worker lifecycle): EVERY native audio call runs inside ONE of
/// the two lifecycle-tracked blocking probes — `completion` (input:
/// default-input lookup + mic test) and `speaker_completion` (output:
/// default-output lookup + production playback smoke). There are deliberately
/// NO separate untracked enumeration `spawn_blocking` workers: a hung native
/// device lookup keeps the shared audio slot occupied exactly like a hung
/// capture, because the same completion that tracks the probe owns it.
pub async fn run_device_check(
    temp_dir: PathBuf,
    event_tx: mpsc::Sender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
    completion: CaptureCompletion,
    speaker_completion: CaptureCompletion,
) -> DeviceCheckResult {
    let mut errors = Vec::new();

    // INPUT probe — tracked by `completion`. The tracked closure performs the
    // default-input lookup AND records the test clip, returning metadata only
    // (RC-1B): the clip is deleted inside the physical closure before
    // Finished is signalled, so this Device Check never owns test-file
    // deletion and an outer abort can never orphan a `device_test_*.wav`. A
    // missing default input surfaces as "No microphone detected"; any other
    // probe failure keeps mic_available true (a mic exists) while failing
    // mic_test_ok so overall readiness still fails.
    let (mic_available, mic_name, mic_test_ok) = match record_test_clip(
        16000,
        1,
        2,
        temp_dir,
        event_tx.clone(),
        stop_flag.clone(),
        completion,
    )
    .await
    {
        Ok(probe) => {
            if !probe.test_ok {
                errors.push("Microphone test recording too short".to_string());
            }
            (true, Some(probe.device_name), probe.test_ok)
        }
        Err(e) => {
            let msg = format!("Mic test failed: {}", e);
            let no_device = msg.to_lowercase().contains("no input device");
            if no_device {
                errors.push("No microphone detected".to_string());
            } else {
                errors.push(msg);
            }
            (!no_device, None, false)
        }
    };

    // RC-2A: a cancelled Device Check must not schedule new physical work.
    // Between the sequential input and output probes, observe the owning
    // command's cancellation so a stop lands promptly and no output stream is
    // started after the input probe was already cancelled.
    if stop_flag.load(std::sync::atomic::Ordering::SeqCst) {
        return DeviceCheckResult {
            mic_available,
            mic_name,
            speaker_available: false,
            speaker_name: None,
            mic_test_ok,
            errors: vec!["Device check cancelled".to_string()],
        };
    }

    // OUTPUT probe — tracked by `speaker_completion`. The tracked closure
    // performs the default-output lookup AND the production playback smoke,
    // returning the tested speaker name. A missing default output is surfaced
    // explicitly; a stream/playback failure fails speaker readiness so the
    // interview cannot start into a first question the candidate could not
    // hear.
    let (speaker_available, speaker_name) =
        match validate_production_playback_stream(speaker_completion).await {
            Ok(name) => (true, Some(name)),
            Err(e) => {
                let msg = e.to_string();
                if msg.to_lowercase().contains("no default output") {
                    errors.push(
                        "No default output device detected — TTS playback has no speaker"
                            .to_string(),
                    );
                } else {
                    errors.push(format!("Speaker not ready for interview playback: {}", msg));
                }
                (false, None)
            }
        };

    DeviceCheckResult {
        mic_available,
        mic_name,
        speaker_available,
        speaker_name,
        mic_test_ok,
        errors,
    }
}
