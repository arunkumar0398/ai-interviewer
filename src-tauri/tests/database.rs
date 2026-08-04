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
