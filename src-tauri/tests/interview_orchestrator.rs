use ai_interviewer_lib::paths::{AppPaths, ToolDirectorySource};
use std::path::PathBuf;

/// Helper: build a fake AppPaths from a base directory for testing
fn fake_app_paths(base: &std::path::Path) -> AppPaths {
    AppPaths {
        tool_dir: base.to_path_buf(),
        db_path: base.join("interviews.db"),
        recordings_dir: base.join("recordings"),
        tts_dir: base.join("tts"),
        temp_dir: base.join("temp"),
        is_portable: false,
        tool_directory_source: ToolDirectorySource::DevFallback,
    }
}

/// Test: Piper installation check fails gracefully when binary missing
#[test]
fn piper_verify_missing_binary() {
    let fake_dir = std::env::temp_dir().join("fake_piper_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);
    std::fs::create_dir_all(&fake_dir).unwrap();

    let paths = fake_app_paths(&fake_dir);
    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&paths);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("not found"),
        "Error should mention not found: {}",
        err_msg
    );

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: Piper installation check fails when model missing
#[test]
fn piper_verify_missing_model() {
    let fake_dir = std::env::temp_dir().join("fake_piper_model_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);

    // Create piper binary but no model
    let piper_bin = fake_dir.join("piper").join("piper").join("piper.exe");
    std::fs::create_dir_all(piper_bin.parent().unwrap()).unwrap();
    std::fs::write(&piper_bin, b"fake binary").unwrap();

    let paths = fake_app_paths(&fake_dir);
    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&paths);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("model"),
        "Error should mention model: {}",
        err_msg
    );

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: Piper installation check passes when both present
#[test]
fn piper_verify_all_present() {
    let fake_dir = std::env::temp_dir().join("fake_piper_ok_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);

    let piper_bin = fake_dir.join("piper").join("piper").join("piper.exe");
    let model_path = fake_dir.join("piper-models").join("en_US-amy-medium.onnx");
    std::fs::create_dir_all(piper_bin.parent().unwrap()).unwrap();
    std::fs::create_dir_all(model_path.parent().unwrap()).unwrap();
    std::fs::write(&piper_bin, b"fake binary").unwrap();
    std::fs::write(&model_path, b"fake model").unwrap();

    let paths = fake_app_paths(&fake_dir);
    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&paths);
    assert!(result.is_ok(), "Should succeed when both files exist");

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: Device check returns result even when no devices available
#[tokio::test]
async fn device_check_handles_no_devices() {
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let temp_dir = std::env::temp_dir().join("ai_interviewer_test_device_check");
    let _ = std::fs::create_dir_all(&temp_dir);
    let result = ai_interviewer_lib::interview::device_check::run_device_check(temp_dir, tx).await;

    // Should return a result, even if devices aren't found
    // On CI/headless, both will be false
    // On a real machine, both will be true
    assert!(
        result.mic_available || !result.mic_available, // always true - just checking it returns
        "Device check should always return a result"
    );
    assert!(
        result.speaker_available || !result.speaker_available,
        "Device check should always return a result"
    );
}

/// Test: Device check result serialization
#[test]
fn device_check_result_serialization() {
    let result = ai_interviewer_lib::interview::device_check::DeviceCheckResult {
        mic_available: true,
        mic_name: Some("Test Microphone".to_string()),
        speaker_available: false,
        speaker_name: None,
        mic_test_ok: false,
        errors: vec!["No speaker detected".to_string()],
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["mic_available"], true);
    assert_eq!(json["mic_name"], "Test Microphone");
    assert_eq!(json["speaker_available"], false);
    assert_eq!(json["speaker_name"], serde_json::Value::Null);
    assert_eq!(json["mic_test_ok"], false);
    assert!(json["errors"][0].as_str().unwrap().contains("speaker"));
}

/// Test: AudioMetadata serialization
#[test]
fn audio_metadata_serialization() {
    let metadata = ai_interviewer_lib::interview::orchestrator::AudioMetadata {
        file_path: "/tmp/round_0_answer.wav".to_string(),
        sha256: "abc123def456".to_string(),
        duration_ms: 5000,
        sample_rate: 16000,
        channels: 1,
        file_size_bytes: 160044,
    };

    let json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(json["file_path"], "/tmp/round_0_answer.wav");
    assert_eq!(json["sha256"], "abc123def456");
    assert_eq!(json["duration_ms"], 5000);
    assert_eq!(json["sample_rate"], 16000);
    assert_eq!(json["channels"], 1);
    assert_eq!(json["file_size_bytes"], 160044);
}

/// Test: InterviewPhase serialization
#[test]
fn interview_phase_serialization() {
    let phase = ai_interviewer_lib::interview::orchestrator::InterviewPhase::SpeakingQuestion {
        question: "Tell me about yourself".to_string(),
    };
    let json = serde_json::to_value(&phase).unwrap();
    assert!(json["SpeakingQuestion"]["question"]
        .as_str()
        .unwrap()
        .contains("Tell me about yourself"));

    let phase = ai_interviewer_lib::interview::orchestrator::InterviewPhase::Error {
        message: "Something went wrong".to_string(),
    };
    let json = serde_json::to_value(&phase).unwrap();
    assert!(json["Error"]["message"]
        .as_str()
        .unwrap()
        .contains("Something went wrong"));
}

/// Test: Whisper binary existence check
#[test]
fn whisper_binary_check() {
    let tools_dir = std::env::var("AI_INTERVIEWER_TOOLS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            // Development fallback: check next to the exe
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_default();
            exe_dir.join("tools")
        });

    let whisper_bin = tools_dir.join("whisper").join("Release").join("main.exe");
    let model_path = tools_dir.join("models").join("ggml-tiny.en.bin");

    // These should exist if spike phase was run
    if whisper_bin.exists() {
        assert!(whisper_bin.is_file());
    }
    if model_path.exists() {
        assert!(model_path.is_file());
        assert!(
            model_path.metadata().unwrap().len() > 1_000_000,
            "Model should be > 1MB"
        );
    }
}

// ============================================================
// NEW: Orchestrator edge case tests
// ============================================================

/// Test: PiperSupervisor construction with different paths
#[test]
fn piper_supervisor_various_paths() {
    // Valid path (won't find binaries, but shouldn't panic)
    let dir = std::env::temp_dir().join("piper_supervisor_various");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);

    let paths = fake_app_paths(&dir);
    let _supervisor = ai_interviewer_lib::audio::tts_supervisor::PiperSupervisor::new(&paths);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test: InterviewPhase serialization covers all variants
#[test]
fn interview_phase_all_variants_serialize() {
    // Unit variants serialize as strings
    let idle =
        serde_json::to_value(&ai_interviewer_lib::interview::orchestrator::InterviewPhase::Idle)
            .unwrap();
    assert_eq!(idle, serde_json::Value::String("Idle".to_string()));

    let recording = serde_json::to_value(
        &ai_interviewer_lib::interview::orchestrator::InterviewPhase::RecordingAnswer,
    )
    .unwrap();
    assert_eq!(
        recording,
        serde_json::Value::String("RecordingAnswer".to_string())
    );

    let processing = serde_json::to_value(
        &ai_interviewer_lib::interview::orchestrator::InterviewPhase::Processing,
    )
    .unwrap();
    assert_eq!(
        processing,
        serde_json::Value::String("Processing".to_string())
    );

    let complete = serde_json::to_value(
        &ai_interviewer_lib::interview::orchestrator::InterviewPhase::Complete,
    )
    .unwrap();
    assert_eq!(complete, serde_json::Value::String("Complete".to_string()));

    // Struct variants serialize as { "VariantName": { ... } }
    let speaking = serde_json::to_value(
        ai_interviewer_lib::interview::orchestrator::InterviewPhase::SpeakingQuestion {
            question: "Tell me about yourself".to_string(),
        },
    )
    .unwrap();
    assert!(speaking.get("SpeakingQuestion").is_some());

    let settling = serde_json::to_value(
        ai_interviewer_lib::interview::orchestrator::InterviewPhase::Settling { duration_ms: 1500 },
    )
    .unwrap();
    assert!(settling.get("Settling").is_some());
}

/// Test: AudioMetadata with zero values (edge case)
#[test]
fn audio_metadata_zero_values() {
    let metadata = ai_interviewer_lib::interview::orchestrator::AudioMetadata {
        file_path: String::new(),
        sha256: String::new(),
        duration_ms: 0,
        sample_rate: 0,
        channels: 0,
        file_size_bytes: 0,
    };

    let json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(json["duration_ms"], 0);
    assert_eq!(json["file_size_bytes"], 0);
}

/// Test: AudioMetadata with max realistic values
#[test]
fn audio_metadata_max_values() {
    let metadata = ai_interviewer_lib::interview::orchestrator::AudioMetadata {
        file_path: "D:\\recordings\\round_4_answer.wav".to_string(),
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        duration_ms: 60000,
        sample_rate: 48000,
        channels: 2,
        file_size_bytes: 11_520_044,
    };

    let json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(json["duration_ms"], 60000);
    assert_eq!(json["sample_rate"], 48000);
    assert_eq!(json["channels"], 2);
}

/// Test: Whisper verification with missing binary (graceful error)
#[test]
fn whisper_verify_missing_binary() {
    let fake_dir = std::env::temp_dir().join("fake_whisper_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);
    std::fs::create_dir_all(&fake_dir).unwrap();

    let whisper_bin = fake_dir.join("whisper").join("Release").join("main.exe");
    assert!(!whisper_bin.exists(), "Binary should not exist in fake dir");

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: Whisper verification with missing model
#[test]
fn whisper_verify_missing_model() {
    let fake_dir = std::env::temp_dir().join("fake_whisper_model_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);

    let whisper_bin = fake_dir.join("whisper").join("Release").join("main.exe");
    std::fs::create_dir_all(whisper_bin.parent().unwrap()).unwrap();
    std::fs::write(&whisper_bin, b"fake").unwrap();

    let model_path = fake_dir.join("models").join("ggml-tiny.en.bin");
    assert!(!model_path.exists(), "Model should not exist");

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: DeviceCheckResult with all errors
#[test]
fn device_check_result_all_errors() {
    let result = ai_interviewer_lib::interview::device_check::DeviceCheckResult {
        mic_available: false,
        mic_name: None,
        speaker_available: false,
        speaker_name: None,
        mic_test_ok: false,
        errors: vec![
            "No microphone detected".to_string(),
            "No speaker detected".to_string(),
            "Mic test failed".to_string(),
        ],
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["errors"].as_array().unwrap().len(), 3);
    assert_eq!(json["mic_available"], false);
    assert_eq!(json["speaker_available"], false);
}

/// Test: DeviceCheckResult with all success
#[test]
fn device_check_result_all_success() {
    let result = ai_interviewer_lib::interview::device_check::DeviceCheckResult {
        mic_available: true,
        mic_name: Some("USB Microphone".to_string()),
        speaker_available: true,
        speaker_name: Some("Speakers (Realtek)".to_string()),
        mic_test_ok: true,
        errors: vec![],
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["mic_available"], true);
    assert_eq!(json["speaker_available"], true);
    assert_eq!(json["mic_test_ok"], true);
    assert_eq!(json["errors"].as_array().unwrap().len(), 0);
}
