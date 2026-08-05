/// Test: Database creation and table initialization
#[test]
fn db_create_and_initialize() {
    let db_path = std::env::temp_dir().join("test_interview.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // Tables should be created
    let session_id = "test-session-001";
    db.create_session(session_id, "John Doe").unwrap();

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session_id);
    assert_eq!(sessions[0].candidate_name, "John Doe");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Insert and retrieve rounds
#[test]
fn db_insert_and_get_rounds() {
    let db_path = std::env::temp_dir().join("test_rounds.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let session_id = "test-session-002";
    db.create_session(session_id, "Jane Smith").unwrap();

    // Insert 3 rounds
    for i in 0..3 {
        db.insert_round(
            session_id,
            i,
            &format!("Question {}", i),
            &format!("Answer {}", i),
            &format!("/tmp/round_{}.wav", i),
            &format!("hash_{}", i),
            5000 + i as u64 * 1000,
            16000,
            1,
            160044,
        )
        .unwrap();
    }

    let rounds = db.get_rounds(session_id).unwrap();
    assert_eq!(rounds.len(), 3);
    assert_eq!(rounds[0].question, "Question 0");
    assert_eq!(rounds[1].question, "Question 1");
    assert_eq!(rounds[2].question, "Question 2");
    assert_eq!(rounds[0].transcription, "Answer 0");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Complete session
#[test]
fn db_complete_session() {
    let db_path = std::env::temp_dir().join("test_complete.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let session_id = "test-session-003";
    db.create_session(session_id, "Test User").unwrap();

    let session = db.get_session(session_id).unwrap().unwrap();
    assert!(session.completed_at.is_none());
    assert_eq!(session.total_rounds, 0);

    db.complete_session(session_id, 5).unwrap();

    let session = db.get_session(session_id).unwrap().unwrap();
    assert!(session.completed_at.is_some());
    assert_eq!(session.total_rounds, 5);

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Database serialization
#[test]
fn db_serialization() {
    let session = ai_interviewer_lib::db::InterviewSession {
        id: "s1".to_string(),
        candidate_name: "Alice".to_string(),
        started_at: "2026-01-01T00:00:00".to_string(),
        completed_at: None,
        total_rounds: 3,
    };
    let json = serde_json::to_value(&session).unwrap();
    assert_eq!(json["id"], "s1");
    assert_eq!(json["candidate_name"], "Alice");

    let round = ai_interviewer_lib::db::InterviewRound {
        id: 1,
        session_id: "s1".to_string(),
        round_index: 0,
        question: "Q1".to_string(),
        transcription: "A1".to_string(),
        audio_path: "/tmp/r1.wav".to_string(),
        sha256: "abc".to_string(),
        duration_ms: 5000,
        sample_rate: 16000,
        channels: 1,
        file_size_bytes: 160044,
        created_at: "2026-01-01T00:00:00".to_string(),
    };
    let json = serde_json::to_value(&round).unwrap();
    assert_eq!(json["question"], "Q1");
    assert_eq!(json["sha256"], "abc");
}

// ============================================================
// NEW: Database edge case tests
// ============================================================

/// Test: Complete a nonexistent session is idempotent (no error, 0 rows affected)
#[test]
fn db_complete_session_nonexistent() {
    let db_path = std::env::temp_dir().join("test_complete_nonexistent.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    let result = db.complete_session("does-not-exist", 5);
    assert!(
        result.is_ok(),
        "Completing nonexistent session should not error"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Get rounds for a session with no rounds returns empty vec
#[test]
fn db_get_rounds_empty_session() {
    let db_path = std::env::temp_dir().join("test_empty_rounds.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("empty-session", "Nobody").unwrap();

    let rounds = db.get_rounds("empty-session").unwrap();
    assert_eq!(
        rounds.len(),
        0,
        "Should return empty vec for session with no rounds"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Get session by ID returns None for nonexistent ID
#[test]
fn db_get_session_nonexistent() {
    let db_path = std::env::temp_dir().join("test_nonexistent_session.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    let result = db.get_session("ghost-session").unwrap();
    assert!(
        result.is_none(),
        "Should return None for nonexistent session"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Multiple sessions are ordered by started_at DESC
#[test]
fn db_sessions_ordered_by_recency() {
    let db_path = std::env::temp_dir().join("test_session_order.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // Insert sessions with slight delay to ensure different timestamps
    db.create_session("first", "Alice").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    db.create_session("second", "Bob").unwrap();

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions[0].id, "second",
        "Most recent session should be first"
    );
    assert_eq!(sessions[1].id, "first");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Duplicate session ID insertion fails (PRIMARY KEY constraint)
#[test]
fn db_duplicate_session_id_fails() {
    let db_path = std::env::temp_dir().join("test_dup_session.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("dup-session", "Alice").unwrap();

    let result = db.create_session("dup-session", "Bob");
    assert!(result.is_err(), "Duplicate session ID should fail");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Round insert returns auto-incremented ID
#[test]
fn db_round_insert_returns_id() {
    let db_path = std::env::temp_dir().join("test_round_id.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("id-session", "Test").unwrap();

    let id1 = db
        .insert_round(
            "id-session",
            0,
            "Q1",
            "A1",
            "/tmp/r1.wav",
            "h1",
            5000,
            16000,
            1,
            160044,
        )
        .unwrap();
    let id2 = db
        .insert_round(
            "id-session",
            1,
            "Q2",
            "A2",
            "/tmp/r2.wav",
            "h2",
            6000,
            16000,
            1,
            192044,
        )
        .unwrap();

    assert!(id2 > id1, "Second round ID should be greater than first");
    assert!(id1 > 0, "IDs should be positive");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Empty candidate name is allowed (empty string default)
#[test]
fn db_empty_candidate_name() {
    let db_path = std::env::temp_dir().join("test_empty_name.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("anon-session", "").unwrap();

    let session = db.get_session("anon-session").unwrap().unwrap();
    assert_eq!(session.candidate_name, "");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Database re-opens cleanly (idempotent initialize)
#[test]
fn db_reopen_idempotent() {
    let db_path = std::env::temp_dir().join("test_reopen.db");
    let _ = std::fs::remove_file(&db_path);

    // Create and populate
    {
        let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
        db.create_session("reopen-session", "Test").unwrap();
    }

    // Re-open — tables already exist, should not fail
    {
        let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
        let sessions = db.get_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "reopen-session");
    }

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Large transcription text round-trips correctly
#[test]
fn db_large_transcription() {
    let db_path = std::env::temp_dir().join("test_large_transcription.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("large-session", "Test").unwrap();

    // 10KB transcription
    let long_text = "word ".repeat(2000);
    db.insert_round(
        "large-session",
        0,
        "Q1",
        &long_text,
        "/tmp/r.wav",
        "h",
        5000,
        16000,
        1,
        160044,
    )
    .unwrap();

    let rounds = db.get_rounds("large-session").unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].transcription.len(), long_text.len());

    let _ = std::fs::remove_file(&db_path);
}
