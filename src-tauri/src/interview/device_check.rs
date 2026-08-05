use crate::audio::capture::{
    list_input_devices, list_output_devices, record_test_clip, CaptureEvent,
};
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
pub async fn run_device_check(event_tx: mpsc::Sender<CaptureEvent>) -> DeviceCheckResult {
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

    // Check output devices
    let output_devices = match list_output_devices().await {
        Ok(devices) => devices,
        Err(e) => {
            errors.push(format!("Failed to list output devices: {}", e));
            Vec::new()
        }
    };

    let speaker_available = !output_devices.is_empty();
    let speaker_name = output_devices.first().cloned();

    if !speaker_available {
        errors.push("No speaker/headphone detected".to_string());
    }

    // Test mic recording (2 second clip)
    let mic_test_ok = if mic_available {
        match record_test_clip(16000, 1, 2, event_tx.clone()).await {
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
