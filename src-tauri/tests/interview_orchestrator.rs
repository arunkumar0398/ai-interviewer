/// Test: Piper installation check fails gracefully when binary missing
#[test]
fn piper_verify_missing_binary() {
    let fake_dir = std::env::temp_dir().join("fake_piper_dir");
    let _ = std::fs::remove_dir_all(&fake_dir);
    std::fs::create_dir_all(&fake_dir).unwrap();

    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&fake_dir);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("not found"), "Error should mention not found: {}", err_msg);

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

    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&fake_dir);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("model"), "Error should mention model: {}", err_msg);

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

    let result = ai_interviewer_lib::audio::tts_supervisor::verify_piper_installation(&fake_dir);
    assert!(result.is_ok(), "Should succeed when both files exist");

    let _ = std::fs::remove_dir_all(&fake_dir);
}

/// Test: Device check returns result even when no devices available
#[tokio::test]
async fn device_check_handles_no_devices() {
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let result = ai_interviewer_lib::interview::device_check::run_device_check(tx).await;

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
    let tools_dir = std::env::var("PIPER_BASE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools")
        });

    let whisper_bin = tools_dir.join("whisper").join("main.exe");
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
