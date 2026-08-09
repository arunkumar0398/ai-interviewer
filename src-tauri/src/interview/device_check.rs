use crate::audio::capture::{
    get_default_output_device_name, list_input_devices, record_test_clip,
    validate_production_playback_stream, CaptureCompletion, CaptureEvent,
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
pub async fn run_device_check(
    temp_dir: PathBuf,
    event_tx: mpsc::Sender<CaptureEvent>,
    stop_flag: Arc<AtomicBool>,
    completion: CaptureCompletion,
) -> DeviceCheckResult {
    let mut errors = Vec::new();

    // Check input devices
    let input_devices = match list_input_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            errors.push(format!("Failed to list input devices: {}", e));
            Vec::new()
        }
    };

    let mic_available = !input_devices.is_empty();
    let mic_name = input_devices.first().cloned();

    if !mic_available {
        errors.push("No microphone detected".to_string());
    }

    // Check the DEFAULT output device. Production playback (Piper TTS and
    // round playback) selects cpal's `default_output_device()`, so enumerating
    // ANY output device is NOT sufficient: a non-default speaker alone must
    // not mark speaker readiness (P2-1).
    let (mut speaker_available, mut speaker_name) = match get_default_output_device_name().await {
        Ok(Some(name)) => (true, Some(name)),
        Ok(None) => {
            errors.push(
                "No default output device detected — TTS playback has no speaker".to_string(),
            );
            (false, None)
        }
        Err(e) => {
            errors.push(format!("Failed to query default output device: {}", e));
            (false, None)
        }
    };

    // RC-4: a default output device existing is NOT enough — production Piper
    // playback must be able to create its exact stream (mono, 22050 Hz) on it.
    // Validate silently and bounded; if the production stream cannot be
    // created, speaker readiness FAILS so the interview cannot start into a
    // first question the candidate could not hear.
    if speaker_available {
        match validate_production_playback_stream().await {
            Ok(_) => {}
            Err(e) => {
                errors.push(format!("Speaker not ready for interview playback: {}", e));
                speaker_available = false;
                speaker_name = None;
            }
        }
    }

    // Test mic recording (2 second clip)
    let mic_test_ok = if mic_available {
        match record_test_clip(
            16000,
            1,
            2,
            temp_dir,
            event_tx.clone(),
            stop_flag,
            completion,
        )
        .await
        {
            Ok(path) => {
                // Check file has reasonable size (at least 1 second of 16kHz 16-bit mono)
                let metadata = std::fs::metadata(&path);
                let ok = metadata
                    .as_ref()
                    .map(|m| m.len() > 32000) // ~1 second at 16kHz 16-bit
                    .unwrap_or(false);

                // Clean up test file
                let _ = std::fs::remove_file(&path);

                if !ok {
                    errors.push("Microphone test recording too short".to_string());
                }
                ok
            }
            Err(e) => {
                errors.push(format!("Mic test failed: {}", e));
                false
            }
        }
    } else {
        false
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
