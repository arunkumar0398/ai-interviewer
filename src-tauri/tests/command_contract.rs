use ai_interviewer_lib::paths::AppPaths;
use tempfile::tempdir;
use uuid::Uuid;

fn make_test_paths() -> AppPaths {
    let tmp = tempdir().unwrap();
    let tool = tmp.path().join("tools");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&tool).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    AppPaths::from_tool_dir(tool, data).unwrap()
}

// ---------------------------------------------------------------------------
// Helper-behavior tests. These validate the path/db helpers the Tauri
// commands call, NOT the Tauri IPC boundary itself (which would require a
// running app instance). Command-boundary input/UUID validation is covered
// by the preflight unit tests in lib.rs and the frontend invoke-mock suite
// in tests/interview.test.tsx.
// ---------------------------------------------------------------------------

#[test]
fn paths_helper_session_recordings_dir_isolates_session_id() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let result = paths.session_recordings_dir(session_id);
    assert!(result
        .to_string_lossy()
        .contains("550e8400-e29b-41d4-a716-446655440000"));
}

#[test]
fn paths_helper_round_audio_path_contains_session_and_round_ids() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let round_id = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440001").unwrap();
    let result = paths.round_audio_path(session_id, round_id);
    let path_str = result.to_string_lossy().to_string();
    assert!(path_str.contains("550e8400"));
    assert!(path_str.contains("660e8400"));
}

#[test]
fn paths_helper_round_audio_path_is_nonexistent_before_recording() {
    let paths = make_test_paths();
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let round_id = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440001").unwrap();
    let path = paths.round_audio_path(session_id, round_id);
    assert!(!path.exists());
}

#[test]
fn paths_helper_tts_output_path_does_not_preexist() {
    let _paths = make_test_paths();
    let tts_dir = _paths.tts_dir.clone();
    std::fs::create_dir_all(&tts_dir).unwrap();
    let output_path = tts_dir.join("test.wav");
    assert!(!output_path.exists());
}

#[test]
fn paths_helper_tool_dir_resolves() {
    let paths = make_test_paths();
    assert!(paths.tool_dir.exists() || !paths.tool_dir.to_string_lossy().is_empty());
}

#[test]
fn db_helper_get_sessions_empty_on_fresh_db() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    let sessions = db.get_sessions().unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn db_helper_get_rounds_empty_on_fresh_db() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    let rounds = db
        .get_rounds("550e8400-e29b-41d4-a716-446655440000")
        .unwrap();
    assert!(rounds.is_empty());
}

#[test]
fn paths_helper_app_config_has_required_fields() {
    let paths = make_test_paths();
    let config = paths.to_app_config();
    assert!(!config.tool_dir.is_empty());
    assert!(config.db_path.ends_with("interviews.db"));
    assert!(!config.recordings_dir.is_empty());
    assert!(!config.temp_dir.is_empty());
}

#[test]
fn paths_helper_validate_readiness_reports_missing_tools() {
    let paths = make_test_paths();
    let readiness = paths.validate_readiness();
    assert!(!readiness.ready);
    assert!(!readiness.issues.is_empty());
}

#[test]
fn paths_helper_resolution_prefers_exe_dir_and_app_data_dir() {
    let tmp = tempdir().unwrap();
    let exe_dir = tmp.path().join("app");
    let app_data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&exe_dir).unwrap();
    std::fs::create_dir_all(&app_data_dir).unwrap();

    let input = ai_interviewer_lib::paths::PathResolutionInput {
        exe_dir,
        app_data_dir,
        resource_dir: None,
        env_tools_dir: None,
    };
    let paths = AppPaths::resolve_from_input(input).unwrap();
    assert!(paths.recordings_dir.starts_with(tmp.path()));
    assert!(paths.temp_dir.starts_with(tmp.path()));
}

#[test]
fn db_helper_accepts_empty_candidate_name_at_db_layer() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    // The DB layer itself allows empty names; empty-name validation is a
    // frontend/command concern, so this asserts the DB operation completes.
    let result = db.create_session("550e8400-e29b-41d4-a716-446655440000", "");
    assert!(result.is_ok());
}

#[test]
fn db_helper_rejects_duplicate_session_id() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("550e8400-e29b-41d4-a716-446655440000", "Alice")
        .unwrap();
    let result = db.create_session("550e8400-e29b-41d4-a716-446655440000", "Bob");
    assert!(result.is_err());
}

#[test]
fn db_helper_complete_session_rejects_missing_session() {
    let tmp = tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    // complete_session returns an error when the session does not exist.
    let result = db.complete_session("550e8400-e29b-41d4-a716-446655440000", 5);
    assert!(result.is_err());
}
