use ai_interviewer_lib::paths::{
    resolve_piper_paths, resolve_whisper_model_path, AppPaths, ToolDirectorySource,
};
use std::fs;

// ---------------------------------------------------------------------------
// resolve_tool_dir integration tests
// ---------------------------------------------------------------------------

/// Test: env var override takes priority when directory exists
#[test]
fn resolve_tool_dir_env_var_priority() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_tools = tmp.path().join("env_tools");
    fs::create_dir_all(&fake_tools).unwrap();

    // Set env var to point to our fake tools dir
    std::env::set_var("AI_INTERVIEWER_TOOLS", fake_tools.to_str().unwrap());

    let (resolved, source, portable) =
        ai_interviewer_lib::paths::resolve_tool_dir(tmp.path().join("exe_dir").as_path());

    // Clean up env var immediately
    std::env::remove_var("AI_INTERVIEWER_TOOLS");

    assert_eq!(resolved, fake_tools);
    assert_eq!(
        source,
        ToolDirectorySource::EnvVar {
            value: fake_tools.to_str().unwrap().to_string()
        }
    );
    assert!(!portable);
}

/// Test: env var pointing to non-existent dir is ignored
#[test]
fn resolve_tool_dir_env_var_nonexistent_falls_through() {
    let tmp = tempfile::tempdir().unwrap();
    let fake_env = tmp.path().join("nonexistent_tools");
    // Don't create it

    std::env::set_var("AI_INTERVIEWER_TOOLS", fake_env.to_str().unwrap());

    let exe_dir = tmp.path().join("exe_dir");
    fs::create_dir_all(&exe_dir).unwrap();

    let (_resolved, _source, _portable) = ai_interviewer_lib::paths::resolve_tool_dir(&exe_dir);

    std::env::remove_var("AI_INTERVIEWER_TOOLS");

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

    // Make sure env var is not set
    std::env::remove_var("AI_INTERVIEWER_TOOLS");

    // Ensure no bundled resources dir
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, portable) = ai_interviewer_lib::paths::resolve_tool_dir(&exe_dir);

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

    std::env::remove_var("AI_INTERVIEWER_TOOLS");

    // Remove any existing resources/tools
    let resources_dir = exe_dir.join("resources").join("tools");
    fs::remove_dir_all(&resources_dir).ok();

    let (resolved, source, portable) = ai_interviewer_lib::paths::resolve_tool_dir(&exe_dir);

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

    let paths = AppPaths::from_tool_dir(tool.clone(), data.clone());
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

    let paths = AppPaths::from_tool_dir(tool, data.clone());
    assert!(paths.db_path.ends_with("interviews.db"));
    assert_eq!(paths.db_path, data.join("interviews.db"));
}

/// Test: database path falls back to legacy when only legacy exists
#[test]
fn app_paths_database_legacy_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    fs::create_dir_all(&tool).unwrap();
    fs::create_dir_all(&data).unwrap();
    // Create only legacy db
    fs::write(data.join("interviewer.db"), b"legacy").unwrap();

    let paths = AppPaths::from_tool_dir(tool, data.clone());
    assert!(paths.db_path.ends_with("interviewer.db"));
    assert_eq!(paths.db_path, data.join("interviewer.db"));
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

    let paths = AppPaths::from_tool_dir(tool, data.clone());
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

    let paths = AppPaths::from_tool_dir(tool, data);
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

    let paths = AppPaths::from_tool_dir(tool, data);
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

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data"));
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

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data"));
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

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data"));
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

    let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data"));
    let readiness = paths.validate_readiness();
    assert!(readiness.ready);
    assert!(readiness.issues.is_empty());
}
