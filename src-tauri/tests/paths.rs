use ai_interviewer_lib::paths::{
    resolve_piper_paths, resolve_whisper_model_path, validate_path_component, AppPaths,
    PathResolutionInput, ToolDirOptions, ToolDirectorySource, ToolDistributionMode,
};
use std::fs;
use uuid::Uuid;

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
        resource_dir: None,
        allow_dev_fallback: false,
    };

    let (resolved, source, mode) = ai_interviewer_lib::paths::resolve_tool_dir_with_options(
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
    assert_eq!(
        mode,
        ToolDistributionMode::EnvVar {
            value: fake_tools.to_str().unwrap().to_string()
        }
    );
}

/// Test: env override pointing to non-existent dir is ignored
#[test]
fn resolve_tool_dir_env_var_nonexistent_falls_through() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_env = tmp.path().join("nonexistent_tools");

    let opts = ToolDirOptions {
        env_override: Some(fake_env.to_str().unwrap().to_string()),
        resource_dir: None,
        allow_dev_fallback: false,
    };

    let exe_dir = tmp.path().join("exe_dir");
    fs::create_dir_all(&exe_dir).unwrap();

    let (_resolved, _source, mode) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    // Should fall through to Unresolved (no tools dir exists next to exe either)
    assert_eq!(mode, ToolDistributionMode::Unresolved);
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
        resource_dir: None,
        allow_dev_fallback: false,
    };

    // Ensure no bundled resources dir
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, mode) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    assert_eq!(resolved, tools_dir);
    assert!(matches!(source, ToolDirectorySource::Portable { .. }));
    assert_eq!(mode, ToolDistributionMode::Portable);
}

/// Test: Unresolved when no tools dir exists anywhere
#[test]
fn resolve_tool_dir_unresolved() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("exe");
    fs::create_dir_all(&exe_dir).unwrap();

    let opts = ToolDirOptions {
        env_override: None,
        resource_dir: None,
        allow_dev_fallback: false,
    };

    // Remove any existing resources/tools
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, mode) =
        ai_interviewer_lib::paths::resolve_tool_dir_with_options(&exe_dir, &opts);

    assert!(resolved.ends_with("tools"));
    assert_eq!(source, ToolDirectorySource::Unresolved);
    assert_eq!(mode, ToolDistributionMode::Unresolved);
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
// UUID validation integration tests (type-level enforcement via Uuid)
// ---------------------------------------------------------------------------

#[test]
fn uuid_parse_accepts_v4_format() {
    assert!(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").is_ok());
    assert!(Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").is_ok());
}

#[test]
fn uuid_parse_rejects_empty() {
    assert!(Uuid::parse_str("").is_err());
}

#[test]
fn uuid_parse_rejects_path_traversal() {
    assert!(Uuid::parse_str("../etc/passwd").is_err());
    assert!(Uuid::parse_str("..\\windows\\system32").is_err());
    assert!(Uuid::parse_str("abc/def").is_err());
    assert!(Uuid::parse_str("abc\\def").is_err());
}

#[test]
fn uuid_parse_rejects_non_uuid_strings() {
    assert!(Uuid::parse_str("not-a-uuid").is_err());
    assert!(Uuid::parse_str("12345").is_err());
    assert!(Uuid::parse_str("abcdefghijklmnopqrstuvwxyz").is_err());
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
// Session-scoped path methods integration tests (typed UUID)
// ---------------------------------------------------------------------------

fn make_test_paths() -> AppPaths {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();
    AppPaths::from_tool_dir(tool, data).unwrap()
}

#[test]
fn session_recordings_dir_returns_correct_path() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let result = paths.session_recordings_dir(session_id);
    let s = result.to_string_lossy();
    assert!(s.contains("550e8400-e29b-41d4-a716-446655440000"));
    assert!(s.contains("recordings"));
}

#[test]
fn session_temp_dir_returns_correct_path() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();
    let result = paths.session_temp_dir(session_id);
    let s = result.to_string_lossy();
    assert!(s.contains("6ba7b810-9dad-11d1-80b4-00c04fd430c8"));
    assert!(s.contains("temp"));
}

#[test]
fn round_audio_path_returns_correct_path() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let round_id = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440001").unwrap();
    let result = paths.round_audio_path(session_id, round_id);
    let s = result.to_string_lossy();
    assert!(s.contains("550e8400"));
    assert!(s.contains("660e8400"));
    assert!(s.ends_with(".wav"));
}

#[test]
fn tts_output_path_returns_correct_path() {
    let paths = make_test_paths();
    let request_id = Uuid::parse_str("770e8400-e29b-41d4-a716-446655440002").unwrap();
    let result = paths.tts_output_path(request_id);
    let s = result.to_string_lossy();
    assert!(s.contains("tts"));
    assert!(s.ends_with(".wav"));
}

#[test]
fn session_recordings_dir_includes_uuid_in_path() {
    let paths = make_test_paths();
    let id = Uuid::new_v4();
    let path = paths.session_recordings_dir(id);
    let expected = ai_interviewer_lib::paths::uuid_to_path(&id);
    assert!(path.to_string_lossy().contains(&expected));
}

// ---------------------------------------------------------------------------
// env_tools_dir override via PathResolutionInput
// ---------------------------------------------------------------------------

/// Test: env_tools_dir in PathResolutionInput takes priority over portable/bundled
#[test]
fn env_tools_dir_override_takes_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_tools = tmp.path().join("env_injected_tools");
    fs::create_dir_all(&fake_tools).unwrap();

    let exe_dir = tmp.path().join("exe");
    let app_data = tmp.path().join("app_data");
    fs::create_dir_all(&exe_dir).unwrap();
    fs::create_dir_all(&app_data).unwrap();

    let input = PathResolutionInput {
        exe_dir,
        app_data_dir: app_data,
        resource_dir: None,
        env_tools_dir: Some(fake_tools.clone()),
    };
    let paths = AppPaths::resolve_from_input(input).unwrap();
    assert_eq!(paths.tool_dir, fake_tools);
    assert_eq!(
        paths.tool_directory_source,
        ToolDirectorySource::EnvVar {
            value: fake_tools.to_str().unwrap().to_string()
        }
    );
}

/// Test: env_tools_dir=None falls through to other resolution strategies
#[test]
fn env_tools_dir_none_falls_through() {
    let tmp = tempfile::tempdir().unwrap();
    let exe_dir = tmp.path().join("exe");
    let tools_dir = exe_dir.join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let app_data = tmp.path().join("app_data");
    fs::create_dir_all(&app_data).unwrap();

    let input = PathResolutionInput {
        exe_dir,
        app_data_dir: app_data,
        resource_dir: None,
        env_tools_dir: None,
    };
    let paths = AppPaths::resolve_from_input(input).unwrap();
    assert_eq!(paths.tool_dir, tools_dir);
    assert!(matches!(
        paths.tool_directory_source,
        ToolDirectorySource::Portable { .. }
    ));
}
