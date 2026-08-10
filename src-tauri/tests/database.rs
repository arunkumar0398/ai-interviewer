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

/// Test: Complete a nonexistent session returns an error
#[test]
fn db_complete_session_nonexistent() {
    let db_path = std::env::temp_dir().join("test_complete_nonexistent.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    let result = db.complete_session("does-not-exist", 5);
    assert!(
        result.is_err(),
        "Completing nonexistent session should error"
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

/// Test: Reusing a session ID for a different candidate fails without changing it.
#[test]
fn db_duplicate_session_id_fails() {
    let db_path = std::env::temp_dir().join("test_dup_session.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("dup-session", "Alice").unwrap();

    let result = db.create_session("dup-session", "Bob");
    let error = result.expect_err("Conflicting session ID reuse should fail");
    match error {
        rusqlite::Error::SqliteFailure(sqlite_error, Some(message)) => {
            assert_eq!(sqlite_error.code, rusqlite::ErrorCode::ConstraintViolation);
            assert!(message.contains("Session ID already exists for a different candidate"));
        }
        other => panic!("Expected a SQLite constraint error, got {other:?}"),
    }

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1, "Conflicting reuse must not add a row");
    assert_eq!(sessions[0].candidate_name, "Alice");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Retrying session creation with the same candidate is idempotent.
#[test]
fn db_create_session_same_id_same_candidate_is_idempotent() {
    let db_path = std::env::temp_dir().join("test_idempotent_session.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("retry-session", "Alice").unwrap();
    db.create_session("retry-session", "Alice").unwrap();

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1, "Retry must not create a second session");
    assert_eq!(sessions[0].id, "retry-session");
    assert_eq!(sessions[0].candidate_name, "Alice");

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

// ============================================================
// DB robustness: WAL, foreign keys, transaction wrapper
// ============================================================

/// Test: WAL mode is enabled after open
#[test]
fn db_wal_mode_enabled() {
    let db_path = std::env::temp_dir().join("test_wal_mode.db");
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("wal-session", "WalTest").unwrap();

    // Check WAL mode via pragma (read-only connection through the same file)
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "WAL mode should be enabled");

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

/// Test: Transaction commits on success
#[test]
fn db_transaction_commits() {
    let db_path = std::env::temp_dir().join("test_txn_commit.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let result = db.in_transaction(|conn| {
        conn.execute(
            "INSERT INTO sessions (id, candidate_name) VALUES (?1, ?2)",
            rusqlite::params!["txn-session", "Txn User"],
        )?;
        Ok(())
    });
    assert!(result.is_ok(), "Transaction should commit");

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "txn-session");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Transaction rolls back on error
#[test]
fn db_transaction_rollback() {
    let db_path = std::env::temp_dir().join("test_txn_rollback.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // First insert succeeds outside transaction
    db.create_session("pre-existing", "Before").unwrap();

    // Transaction fails — should rollback
    let result: Result<(), rusqlite::Error> = db.in_transaction(|conn| {
        conn.execute(
            "INSERT INTO sessions (id, candidate_name) VALUES (?1, ?2)",
            rusqlite::params!["txn-fail", "During"],
        )?;
        // Force an error (duplicate PK)
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("UNIQUE constraint failed".into()),
        ))
    });
    assert!(result.is_err(), "Transaction should rollback on error");

    // Only pre-existing session should remain
    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "pre-existing");

    let _ = std::fs::remove_file(&db_path);
}

// ============================================================
// Schema versioning and unique round constraint
// ============================================================

/// Test: Schema version is set to current version after initialize
#[test]
fn db_schema_version_set() {
    let db_path = std::env::temp_dir().join("test_schema_version.db");
    let _ = std::fs::remove_file(&db_path);

    let _db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2, "Schema version should be 2");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Duplicate (session_id, round_index) returns an error (no longer silently ignored)
#[test]
fn db_duplicate_round_index_errors() {
    let db_path = std::env::temp_dir().join("test_dup_round.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("dup-round-session", "Test").unwrap();

    let outcome = db.insert_round_with_session_update(
        "dup-round-session",
        0,
        "Q1",
        "A1",
        "/tmp/r1.wav",
        "h1",
        5000,
        16000,
        1,
        160044,
        false,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    // Second insert with same (session_id, round_index) should fail with UNIQUE error
    let result = db.insert_round_with_session_update(
        "dup-round-session",
        0,
        "Q1-retry",
        "A1-retry",
        "/tmp/r1b.wav",
        "h1b",
        5500,
        16000,
        1,
        168044,
        false,
    );
    assert!(
        matches!(
            result,
            ai_interviewer_lib::db::PersistenceOutcome::NotCommitted { .. }
        ),
        "Duplicate round_index should be conclusively NotCommitted"
    );

    let rounds = db.get_rounds("dup-round-session").unwrap();
    assert_eq!(rounds.len(), 1, "Only original round should exist");
    assert_eq!(rounds[0].question, "Q1", "Original round should be kept");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: insert_round_with_session_update increments total_rounds
#[test]
fn db_insert_round_with_session_update() {
    let db_path = std::env::temp_dir().join("test_round_session_update.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("update-session", "Test").unwrap();

    assert_eq!(
        db.get_session("update-session")
            .unwrap()
            .unwrap()
            .total_rounds,
        0
    );

    let outcome = db.insert_round_with_session_update(
        "update-session",
        0,
        "Q1",
        "A1",
        "/tmp/r1.wav",
        "h1",
        5000,
        16000,
        1,
        160044,
        false,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    assert_eq!(
        db.get_session("update-session")
            .unwrap()
            .unwrap()
            .total_rounds,
        1
    );

    let outcome = db.insert_round_with_session_update(
        "update-session",
        1,
        "Q2",
        "A2",
        "/tmp/r2.wav",
        "h2",
        6000,
        16000,
        1,
        192044,
        false,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    assert_eq!(
        db.get_session("update-session")
            .unwrap()
            .unwrap()
            .total_rounds,
        2
    );

    let rounds = db.get_rounds("update-session").unwrap();
    assert_eq!(rounds.len(), 2);

    let _ = std::fs::remove_file(&db_path);
}

/// Test: insert_round_with_session_update rolls back atomically on error
#[test]
fn db_insert_round_with_session_update_rollback() {
    let db_path = std::env::temp_dir().join("test_round_rollback.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("rollback-session", "Test").unwrap();

    // First insert succeeds
    let outcome = db.insert_round_with_session_update(
        "rollback-session",
        0,
        "Q1",
        "A1",
        "/tmp/r1.wav",
        "h1",
        5000,
        16000,
        1,
        160044,
        false,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    assert_eq!(
        db.get_session("rollback-session")
            .unwrap()
            .unwrap()
            .total_rounds,
        1
    );

    // Duplicate round_index should error (UNIQUE constraint), and total_rounds
    // should NOT increase because the transaction rolls back.
    let result = db.insert_round_with_session_update(
        "rollback-session",
        0,
        "Q1-dup",
        "A1-dup",
        "/tmp/r1b.wav",
        "h1b",
        5500,
        16000,
        1,
        168044,
        false,
    );
    assert!(
        matches!(
            result,
            ai_interviewer_lib::db::PersistenceOutcome::NotCommitted { .. }
        ),
        "Duplicate round_index should be conclusively NotCommitted"
    );
    assert_eq!(
        db.get_session("rollback-session")
            .unwrap()
            .unwrap()
            .total_rounds,
        1,
        "total_rounds should not increase for failed duplicate"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: a final round commits the round AND the session completion together.
/// completed_at must be set in the same transaction as the round insert.
#[test]
fn db_final_round_commits_round_and_session_completion() {
    let db_path = std::env::temp_dir().join("test_final_round_commit.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("final-session", "Test").unwrap();

    let session_before = db.get_session("final-session").unwrap().unwrap();
    assert!(session_before.completed_at.is_none());

    let outcome = db.insert_round_with_session_update(
        "final-session",
        0,
        "Q-final",
        "A-final",
        "/tmp/rf.wav",
        "hf",
        7000,
        16000,
        1,
        224044,
        true,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    // Round persisted and session finalized together.
    let rounds = db.get_rounds("final-session").unwrap();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].question, "Q-final");

    let session_after = db.get_session("final-session").unwrap().unwrap();
    assert_eq!(session_after.total_rounds, 1);
    assert!(
        session_after.completed_at.is_some(),
        "completed_at must be set when the final round commits"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: a non-final round must NOT mark the session completed.
#[test]
fn db_non_final_round_does_not_complete_session() {
    let db_path = std::env::temp_dir().join("test_non_final_round.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    db.create_session("nonfinal-session", "Test").unwrap();

    let outcome = db.insert_round_with_session_update(
        "nonfinal-session",
        0,
        "Q1",
        "A1",
        "/tmp/r1.wav",
        "h1",
        5000,
        16000,
        1,
        160044,
        false,
    );
    assert!(
        matches!(
            outcome,
            ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
        ),
        "round insert must commit"
    );
    let session = db.get_session("nonfinal-session").unwrap().unwrap();
    assert_eq!(session.total_rounds, 1);
    assert!(
        session.completed_at.is_none(),
        "completed_at must stay NULL for non-final rounds"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: when the session does not exist, the whole transaction rolls back and
/// no round row is left behind.
#[test]
fn db_final_round_missing_session_rolls_back_everything() {
    let db_path = std::env::temp_dir().join("test_missing_session_rollback.db");
    let _ = std::fs::remove_file(&db_path);

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let result = db.insert_round_with_session_update(
        "no-such-session",
        0,
        "Q1",
        "A1",
        "/tmp/r1.wav",
        "h1",
        5000,
        16000,
        1,
        160044,
        true,
    );
    assert!(
        matches!(
            result,
            ai_interviewer_lib::db::PersistenceOutcome::NotCommitted { .. }
        ),
        "Insert for a missing session must be conclusively NotCommitted"
    );

    let rounds = db.get_rounds("no-such-session").unwrap();
    assert!(
        rounds.is_empty(),
        "No partially persisted round may remain after a rolled-back transaction"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Test: A database created by a newer build (future schema version) is
/// rejected without any mutation — no downgrade migration, no user_version
/// write, no data/schema change, no journal-mode switch.
#[test]
fn db_future_schema_version_rejected_without_mutation() {
    let db_path = std::env::temp_dir().join("test_future_schema.db");
    let _ = std::fs::remove_file(&db_path);

    // Create a database with a FUTURE schema version and real data.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=3;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE future_table (
                id INTEGER PRIMARY KEY,
                payload TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        conn.execute("INSERT INTO future_table (payload) VALUES ('keep-me')", [])
            .unwrap();
    }

    // Open must FAIL: version 3 is newer than supported version 2.
    let err_msg = match ai_interviewer_lib::db::Database::open(&db_path) {
        Ok(_) => panic!("Opening a future-version database must error"),
        Err(e) => e.to_string(),
    };
    assert!(
        err_msg.contains("newer than supported version"),
        "error should be explicit, got: {}",
        err_msg
    );

    // user_version must still be 3 — no silent downgrade happened.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3, "user_version must remain 3 (no downgrade)");

    // Data/schema unchanged.
    let payload: String = conn
        .query_row("SELECT payload FROM future_table WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(payload, "keep-me", "data must remain untouched");

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

/// Test: a database at the CURRENT schema version opens fine (no regression).
#[test]
fn db_current_schema_version_opens() {
    let db_path = std::env::temp_dir().join("test_current_schema.db");
    let _ = std::fs::remove_file(&db_path);

    // Pre-create with user_version=2 (current) and the v2 schema.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=2;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                candidate_name TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT,
                total_rounds INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE rounds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                question TEXT NOT NULL,
                transcription TEXT NOT NULL DEFAULT '',
                audio_path TEXT NOT NULL DEFAULT '',
                sha256 TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                sample_rate INTEGER NOT NULL DEFAULT 16000,
                channels INTEGER NOT NULL DEFAULT 1,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(session_id, round_index),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            ",
        )
        .unwrap();
    }

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();
    // No migration should have run; a session can be created and read back.
    db.create_session("current-schema-session", "Test").unwrap();
    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1);

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

/// Test: Migration from schema v1 to v2 preserves data and adds unique constraint
#[test]
fn db_schema_v1_to_v2_migration() {
    let db_path = std::env::temp_dir().join("test_migration_v1_v2.db");
    let _ = std::fs::remove_file(&db_path);

    // Create a v1 database manually (no UNIQUE constraint on rounds)
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=1;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                candidate_name TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT,
                total_rounds INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE rounds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                question TEXT NOT NULL,
                transcription TEXT NOT NULL DEFAULT '',
                audio_path TEXT NOT NULL DEFAULT '',
                sha256 TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                sample_rate INTEGER NOT NULL DEFAULT 16000,
                channels INTEGER NOT NULL DEFAULT 1,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_rounds_session ON rounds(session_id);
            ",
        )
        .unwrap();

        // Insert test data
        conn.execute(
            "INSERT INTO sessions (id, candidate_name, total_rounds) VALUES ('s1', 'Alice', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 0, 'Q1', 'A1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 1, 'Q2', 'A2')",
            [],
        )
        .unwrap();
    }

    // Open with the real Database::open — triggers migration v1 -> v2
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // Data should be preserved
    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "s1");
    assert_eq!(sessions[0].candidate_name, "Alice");
    assert_eq!(sessions[0].total_rounds, 2);

    let rounds = db.get_rounds("s1").unwrap();
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].question, "Q1");
    assert_eq!(rounds[1].question, "Q2");

    let _ = std::fs::remove_file(&db_path);
}

/// Test: Migration from v1 with duplicate rounds keeps highest-id per (session_id, round_index)
#[test]
fn db_schema_v1_to_v2_migration_deterministic_tiebreak() {
    let db_path = std::env::temp_dir().join("test_migration_v1_v2_tiebreak.db");
    let _ = std::fs::remove_file(&db_path);

    // Create a v1 database with duplicate (session_id, round_index) rows
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=1;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                candidate_name TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT,
                total_rounds INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE rounds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                question TEXT NOT NULL,
                transcription TEXT NOT NULL DEFAULT '',
                audio_path TEXT NOT NULL DEFAULT '',
                sha256 TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                sample_rate INTEGER NOT NULL DEFAULT 16000,
                channels INTEGER NOT NULL DEFAULT 1,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_rounds_session ON rounds(session_id);
            ",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO sessions (id, candidate_name, total_rounds) VALUES ('s1', 'Alice', 3)",
            [],
        )
        .unwrap();
        // Insert 3 rows for round_index=0: ids 1, 2, 3 — keep id=3
        conn.execute(
            "INSERT INTO rounds (id, session_id, round_index, question, transcription) VALUES (1, 's1', 0, 'Q1-old', 'A1-old')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (id, session_id, round_index, question, transcription) VALUES (2, 's1', 0, 'Q1-mid', 'A1-mid')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (id, session_id, round_index, question, transcription) VALUES (3, 's1', 0, 'Q1-new', 'A1-new')",
            [],
        )
        .unwrap();
        // Insert 2 rows for round_index=1: ids 4, 5 — keep id=5
        conn.execute(
            "INSERT INTO rounds (id, session_id, round_index, question, transcription) VALUES (4, 's1', 1, 'Q2-old', 'A2-old')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (id, session_id, round_index, question, transcription) VALUES (5, 's1', 1, 'Q2-new', 'A2-new')",
            [],
        )
        .unwrap();
    }

    // Open with the real Database::open — triggers migration v1 -> v2
    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // Data should be preserved, highest-id rows kept
    let rounds = db.get_rounds("s1").unwrap();
    assert_eq!(
        rounds.len(),
        2,
        "Should have 2 unique rounds after migration"
    );
    assert_eq!(
        rounds[0].question, "Q1-new",
        "Should keep highest-id row for round_index=0"
    );
    assert_eq!(
        rounds[1].question, "Q2-new",
        "Should keep highest-id row for round_index=1"
    );
    // Surviving physical rows are exactly the highest ids (3 and 5).
    assert_eq!(rounds[0].id, 3, "round_index=0 keeps id=3");
    assert_eq!(rounds[1].id, 5, "round_index=1 keeps id=5");
    // P1-1: total_rounds is reconciled to the LOGICAL count after
    // deduplication — 5 physical rows collapse into 2 logical rounds, so the
    // stale stored counter (3) must become 2.
    let sessions = db.get_sessions().unwrap();
    assert_eq!(
        sessions[0].total_rounds, 2,
        "total_rounds must match logical round count"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// P1-1 Case A: a v1 session with a STALE total_rounds (0) and two persisted
/// rounds migrates to total_rounds == 2 — the counter is reconciled inside the
/// same migration transaction.
#[test]
fn db_schema_v1_to_v2_reconciles_stale_total_rounds() {
    let db_path = std::env::temp_dir().join("test_migration_v1_v2_stale_count.db");
    let _ = std::fs::remove_file(&db_path);

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=1;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                candidate_name TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT,
                total_rounds INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE rounds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                question TEXT NOT NULL,
                transcription TEXT NOT NULL DEFAULT '',
                audio_path TEXT NOT NULL DEFAULT '',
                sha256 TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                sample_rate INTEGER NOT NULL DEFAULT 16000,
                channels INTEGER NOT NULL DEFAULT 1,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            ",
        )
        .unwrap();

        // Stale counter: total_rounds = 0 while two rounds exist.
        conn.execute(
            "INSERT INTO sessions (id, candidate_name, total_rounds) VALUES ('s1', 'Alice', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 0, 'Q1', 'A1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 1, 'Q2', 'A2')",
            [],
        )
        .unwrap();
    }

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    let sessions = db.get_sessions().unwrap();
    assert_eq!(sessions[0].total_rounds, 2, "stale 0 must reconcile to 2");
    let rounds = db.get_rounds("s1").unwrap();
    assert_eq!(rounds.len(), 2, "both rounds remain");

    // Schema version must be 2 after the migration.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2, "user_version must become 2");

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}

/// P1-1 Case C: after migrating rounds [0, 1] (with a stale counter), the
/// session continues through rounds 2, 3, 4 — the final session must end with
/// total_rounds == 5 and completed_at populated.
#[test]
fn db_schema_v1_to_v2_reconciled_count_continues_to_completion() {
    let db_path = std::env::temp_dir().join("test_migration_v1_v2_continue.db");
    let _ = std::fs::remove_file(&db_path);

    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version=1;").unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                candidate_name TEXT NOT NULL DEFAULT '',
                started_at TEXT NOT NULL DEFAULT (datetime('now')),
                completed_at TEXT,
                total_rounds INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE rounds (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                question TEXT NOT NULL,
                transcription TEXT NOT NULL DEFAULT '',
                audio_path TEXT NOT NULL DEFAULT '',
                sha256 TEXT NOT NULL DEFAULT '',
                duration_ms INTEGER NOT NULL DEFAULT 0,
                sample_rate INTEGER NOT NULL DEFAULT 16000,
                channels INTEGER NOT NULL DEFAULT 1,
                file_size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
            );
            ",
        )
        .unwrap();

        // Stale counter (0) with rounds [0, 1].
        conn.execute(
            "INSERT INTO sessions (id, candidate_name, total_rounds) VALUES ('s1', 'Alice', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 0, 'Q1', 'A1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription) VALUES ('s1', 1, 'Q2', 'A2')",
            [],
        )
        .unwrap();
    }

    let db = ai_interviewer_lib::db::Database::open(&db_path).unwrap();

    // Reconcile happened: counter starts at 2, not 0.
    assert_eq!(db.get_session("s1").unwrap().unwrap().total_rounds, 2);

    // Complete rounds 2, 3, 4 (final).
    for (index, is_final) in [(2, false), (3, false), (4, true)] {
        let outcome = db.insert_round_with_session_update(
            "s1",
            index,
            &format!("Q{}", index + 1),
            &format!("A{}", index + 1),
            "/tmp/x.wav",
            "hash",
            5000,
            16000,
            1,
            160044,
            is_final,
        );
        assert!(
            matches!(
                outcome,
                ai_interviewer_lib::db::PersistenceOutcome::Committed(_)
            ),
            "round insert must commit"
        );
    }

    let session = db.get_session("s1").unwrap().unwrap();
    assert_eq!(db.get_rounds("s1").unwrap().len(), 5, "5 rounds persisted");
    assert_eq!(session.total_rounds, 5, "total_rounds must end at 5");
    assert!(
        session.completed_at.is_some(),
        "completed_at must be populated after the final round"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
}
