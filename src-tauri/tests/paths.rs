use ai_interviewer_lib::paths::{
    resolve_piper_paths, resolve_whisper_model_path, validate_path_component, validate_uuid,
    AppPaths, ToolDirOptions, ToolDirectorySource,
};
use std::fs;

// ---------------------------------------------------------------------------
// resolve_tool_dir_with_options integration tests
// ---------------------------------------------------------------------------

/// Test: env override takes priority when directory exists
#[test]
fn resolve_tool_dir_env_var_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_tools = tmp.path().join("env_tools");
    fs::create_dir_all(&fake_tools).unwrap();

    let opts = ToolDirOptions {
        env_override: Some(fake_tools.to_str().unwrap().to_string()),
        allow_dev_fallback: false,
    };

    let (resolved, source, portable) = ai_interviewer_lib::paths::resolve_tool_dir_with_options(
        tmp.path().join("exe_dir").as_path(),
        &opts,
    );

    assert_eq!(resolved, fake_tools);
    assert_eq!(
        source,
        ToolDirectorySource::EnvVar {
            value: fake_tools.to_str().unwrap().to_string()
        }
    );
    assert!(!portable);
}

/// Test: env override pointing to non-existent dir is ignored
#[test]
fn resolve_tool_dir_env_var_nonexistent_falls_through() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_env = tmp.path().join("nonexistent_tools");

    let opts = ToolDirOptions {
        env_override: Some(fake_env.to_str().unwrap().to_string()),
        allow_dev_fallback: false,
    };

    let exe_dir = tmp.path().join("exe_dir");
    fs::create_dir_all(&exe_dir).unwrap();

    let (_resolved, _source, _portable) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    // Should fall through to Unresolved (no tools dir exists next to exe either)
    // The resolved path should be exe_dir/tools
}

/// Test: portable layout detected when tools dir is next to exe
#[test]
fn resolve_tool_dir_portable_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("exe");
    let tools_dir = exe_dir.join("tools");
    fs::create_dir_all(&tools_dir).unwrap();

    let opts = ToolDirOptions {
        env_override: None,
        allow_dev_fallback: false,
    };

    // Ensure no bundled resources dir
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, portable) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    assert_eq!(resolved, tools_dir);
    assert!(matches!(source, ToolDirectorySource::Portable { .. }));
    assert!(portable);
}

/// Test: Unresolved when no tools dir exists anywhere
#[test]
fn resolve_tool_dir_unresolved() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("exe");
    fs::create_dir_all(&exe_dir).unwrap();

    let opts = ToolDirOptions {
        env_override: None,
        allow_dev_fallback: false,
    };

    // Remove any existing resources/tools
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, portable) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    assert!(resolved.ends_with("tools"));
    assert_eq!(source, ToolDirectorySource::Unresolved);
    assert!(!portable);
}

// ---------------------------------------------------------------------------
// AppPaths integration tests
// ---------------------------------------------------------------------------

/// Test: AppPaths::from_tool_dir produces correct layout
#[test]
fn app_paths_from_tool_dir_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();

    let paths = AppPaths::from_tool_dir(tool.clone(), data.clone()).unwrap();
    assert_eq!(paths.tool_dir, tool);
    assert!(paths.db_path.ends_with("interviews.db"));
    assert_eq!(paths.recordings_dir, data.join("recordings"));
    assert_eq!(paths.temp_dir, data.join("temp"));
    assert!(!paths.is_portable);
    assert_eq!(
        paths.tool_directory_source,
        ToolDirectorySource::DevFallback
    );
}

/// Test: database path uses canonical name when neither file exists
#[test]
fn app_paths_database_canonical_when_new() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();

    let paths = AppPaths::from_tool_dir(tool, data.clone()).unwrap();
    assert!(paths.db_path.ends_with("interviews.db"));
    assert_eq!(paths.db_path, data.join("interviews.db"));
}

/// Test: database path migrates legacy to canonical when only legacy exists
#[test]
fn app_paths_database_legacy_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();
    // Create only legacy db
    fs::write(data.join("interviewer.db"), b"legacy").unwrap();

    let paths = AppPaths::from_tool_dir(tool, data.clone()).unwrap();
    // After migration, the canonical path is used and legacy is renamed
    assert!(paths.db_path.ends_with("interviews.db"));
    assert_eq!(paths.db_path, data.join("interviews.db"));
    // Legacy file no longer exists after migration
    assert!(!data.join("interviewer.db").exists());
}

/// Test: database path prefers canonical when both exist
#[test]
fn app_paths_database_prefers_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("interviews.db"), b"canonical").unwrap();
    fs::write(data.join("interviewer.db"), b"legacy").unwrap();

    let paths = AppPaths::from_tool_dir(tool, data.clone()).unwrap();
    assert!(paths.db_path.ends_with("interviews.db"));
    assert_eq!(paths.db_path, data.join("interviews.db"));
}

/// Test: ensure_directories creates both recordings and temp dirs
#[test]
fn app_paths_ensure_directories_creates_all() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();

    let paths = AppPaths::from_tool_dir(tool, data).unwrap();
    assert!(!paths.recordings_dir.exists());
    assert!(!paths.temp_dir.exists());

    paths.ensure_directories().unwrap();
    assert!(paths.recordings_dir.exists());
    assert!(paths.temp_dir.exists());
}

/// Test: to_app_config produces valid JSON with all required fields
#[test]
fn app_paths_to_app_config_json() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();

    let paths = AppPaths::from_tool_dir(tool, data).unwrap();
    let config = paths.to_app_config();
    let json = serde_json::to_value(&config).unwrap();

    assert!(json.get("tool_dir").is_some());
    assert!(json.get("db_path").is_some());
    assert!(json.get("recordings_dir").is_some());
    assert!(json.get("temp_dir").is_some());
    assert!(json.get("is_portable").is_some());
    assert!(json.get("tool_directory_source").is_some());
    assert!(json.get("readiness").is_some());
    assert!(json["readiness"].get("ready").is_some());
    assert!(json["readiness"].get("issues").is_some());
}

// ---------------------------------------------------------------------------
// resolve_piper_paths integration tests
// ---------------------------------------------------------------------------

/// Test: piper resolution prefers canonical over legacy
#[test]
fn resolve_piper_prefers_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");

    // Create both canonical and legacy
    fs::create_dir_all(tool.join("piper")).unwrap();
    fs::write(tool.join("piper").join("piper.exe"), b"canonical").unwrap();
    fs::write(tool.join("piper").join("model.onnx"), b"canonical").unwrap();
    fs::create_dir_all(tool.join("piper").join("piper")).unwrap();
    fs::write(
        tool.join("piper").join("piper").join("piper.exe"),
        b"legacy",
    )
    .unwrap();
    fs::create_dir_all(tool.join("piper-models")).unwrap();
    fs::write(
        tool.join("piper-models").join("en_US-amy-medium.onnx"),
        b"legacy",
    )
    .unwrap();

    let (bin, model) = resolve_piper_paths(&tool);
    assert_eq!(bin, Some(tool.join("piper").join("piper.exe")));
    assert_eq!(model, Some(tool.join("piper").join("model.onnx")));
}

// ---------------------------------------------------------------------------
// resolve_whisper_* integration tests
// ---------------------------------------------------------------------------

/// Test: whisper model resolution
#[test]
fn resolve_whisper_model_found() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");

    fs::create_dir_all(tool.join("models")).unwrap();
    fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"model").unwrap();

    assert_eq!(
        resolve_whisper_model_path(&tool),
        Some(tool.join("models").join("ggml-tiny.en.bin"))
    );
}

/// Test: whisper model missing
#[test]
fn resolve_whisper_model_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    fs::create_dir_all(&tool).unwrap();

    assert_eq!(resolve_whisper_model_path(&tool), None);
}

// ---------------------------------------------------------------------------
// validate_readiness integration tests
// ---------------------------------------------------------------------------

/// Test: readiness reports all issues when nothing exists
#[test]
fn validate_readiness_all_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    fs::create_dir_all(&tool).unwrap();

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
    let readiness = paths.validate_readiness();
    assert!(!readiness.ready);
    // Should have at least: PIPER_BINARY_MISSING, WHISPER_BINARY_MISSING, PIPER_MODEL_MISSING, WHISPER_MODEL_MISSING
    let codes: Vec<&str> = readiness.issues.iter().map(|i| i.code.as_str()).collect();
    assert!(codes.contains(&"PIPER_BINARY_MISSING"));
    assert!(codes.contains(&"WHISPER_BINARY_MISSING"));
    assert!(codes.contains(&"PIPER_MODEL_MISSING"));
    assert!(codes.contains(&"WHISPER_MODEL_MISSING"));
}

/// Test: readiness reports partial issues
#[test]
fn validate_readiness_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");

    // Only whisper exists
    fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
    fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
    fs::create_dir_all(tool.join("models")).unwrap();
    fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
    let readiness = paths.validate_readiness();
    assert!(!readiness.ready);
    let codes: Vec<&str> = readiness.issues.iter().map(|i| i.code.as_str()).collect();
    assert!(codes.contains(&"PIPER_BINARY_MISSING"));
    assert!(codes.contains(&"PIPER_MODEL_MISSING"));
    assert!(!codes.contains(&"WHISPER_BINARY_MISSING"));
    assert!(!codes.contains(&"WHISPER_MODEL_MISSING"));
}

/// Test: readiness passes with canonical layout
#[test]
fn validate_readiness_canonical_all_present() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");

    fs::create_dir_all(tool.join("piper")).unwrap();
    fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
    fs::write(tool.join("piper").join("model.onnx"), b"").unwrap();
    fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
    fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
    fs::create_dir_all(tool.join("models")).unwrap();
    fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
    let readiness = paths.validate_readiness();
    assert!(readiness.ready);
    assert!(readiness.issues.is_empty());
}

/// Test: readiness passes with legacy layout
#[test]
fn validate_readiness_legacy_all_present() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");

    // Legacy piper: piper/piper/piper.exe
    fs::create_dir_all(tool.join("piper").join("piper")).unwrap();
    fs::write(tool.join("piper").join("piper").join("piper.exe"), b"").unwrap();
    // Legacy model: piper-models/en_US-amy-medium.onnx
    fs::create_dir_all(tool.join("piper-models")).unwrap();
    fs::write(tool.join("piper-models").join("en_US-amy-medium.onnx"), b"").unwrap();
    // Whisper + model
    fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
    fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
    fs::create_dir_all(tool.join("models")).unwrap();
    fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
    let readiness = paths.validate_readiness();
    assert!(readiness.ready);
    assert!(readiness.issues.is_empty());
}

// ---------------------------------------------------------------------------
// UUID validation integration tests
// ---------------------------------------------------------------------------

#[test]
fn uuid_validation_accepts_v4_format() {
    assert!(validate_uuid("550e8400-e29b-41d4-a716-446655440000").is_ok());
    assert!(validate_uuid("6ba7b810-9dad-11d1-80b4-00c04fd430c8").is_ok());
}

#[test]
fn uuid_validation_rejects_empty() {
    assert!(validate_uuid("").is_err());
}

#[test]
fn uuid_validation_rejects_path_traversal() {
    assert!(validate_uuid("../etc/passwd").is_err());
    assert!(validate_uuid("..\\windows\\system32").is_err());
    assert!(validate_uuid("abc/def").is_err());
    assert!(validate_uuid("abc\\def").is_err());
}

#[test]
fn uuid_validation_rejects_non_uuid_strings() {
    assert!(validate_uuid("not-a-uuid").is_err());
    assert!(validate_uuid("12345").is_err());
    assert!(validate_uuid("abcdefghijklmnopqrstuvwxyz").is_err());
}

// ---------------------------------------------------------------------------
// Path component validation integration tests
// ---------------------------------------------------------------------------

#[test]
fn path_component_validation_rejects_dotdot() {
    assert!(validate_path_component("..").is_err());
    assert!(validate_path_component(".").is_err());
}

#[test]
fn path_component_validation_rejects_slashes_and_null() {
    assert!(validate_path_component("a/b").is_err());
    assert!(validate_path_component("a\\b").is_err());
    assert!(validate_path_component("a\0b").is_err());
}

#[test]
fn path_component_validation_accepts_normal() {
    assert!(validate_path_component("my-session").is_ok());
    assert!(validate_path_component("abc123").is_ok());
}

// ---------------------------------------------------------------------------
// Session-scoped path methods integration tests
// ---------------------------------------------------------------------------

#[test]
fn session_recordings_dir_rejects_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    // Traversal attempts should be rejected
    assert!(paths.session_recordings_dir("../etc/passwd").is_err());
    assert!(paths.session_recordings_dir("../../../root").is_err());
    assert!(paths.session_recordings_dir("abc/def").is_err());
}

#[test]
fn session_recordings_dir_rejects_non_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    assert!(paths.session_recordings_dir("not-a-uuid").is_err());
    assert!(paths.session_recordings_dir("").is_err());
}

#[test]
fn session_recordings_dir_accepts_valid_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    let result = paths.session_recordings_dir("550e8400-e29b-41d4-a716-446655440000");
    assert!(result.is_ok());
    assert!(result
        .unwrap()
        .ends_with("550e8400-e29b-41d4-a716-446655440000"));
}

#[test]
fn session_temp_dir_rejects_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    assert!(paths.session_temp_dir("../etc/passwd").is_err());
    assert!(paths.session_temp_dir("abc/def").is_err());
}

#[test]
fn session_temp_dir_accepts_valid_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    let result = paths.session_temp_dir("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
    assert!(result.is_ok());
    assert!(result
        .unwrap()
        .ends_with("6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
}

#[test]
fn round_audio_path_rejects_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    assert!(paths
        .round_audio_path("../etc/passwd", "550e8400-e29b-41d4-a716-446655440000")
        .is_err());
    assert!(paths
        .round_audio_path("550e8400-e29b-41d4-a716-446655440000", "../../escape")
        .is_err());
}

#[test]
fn round_audio_path_accepts_valid_uuids() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    let result = paths.round_audio_path(
        "550e8400-e29b-41d4-a716-446655440000",
        "660e8400-e29b-41d4-a716-446655440001",
    );
    assert!(result.is_ok());
    let path = result.unwrap();
    assert!(path.to_string_lossy().contains("550e8400"));
    assert!(path.to_string_lossy().contains("660e8400"));
    assert!(path.to_string_lossy().ends_with(".wav"));
}

#[test]
fn tts_output_path_rejects_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    assert!(paths.tts_output_path("../escape").is_err());
}

#[test]
fn tts_output_path_accepts_valid_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    let paths = AppPaths::from_tool_dir(tool, data).unwrap();

    let result = paths.tts_output_path("770e8400-e29b-41d4-a716-446655440002");
    assert!(result.is_ok());
    let path = result.unwrap();
    assert!(path.to_string_lossy().contains("tts"));
    assert!(path.to_string_lossy().ends_with(".wav"));
}
