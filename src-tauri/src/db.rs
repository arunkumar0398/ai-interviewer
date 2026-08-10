use rusqlite::{params, Connection, Result as SqlResult, TransactionBehavior};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA_VERSION: u32 = 2;

/// Database wrapper for interview data storage
pub struct Database {
    conn: Mutex<Connection>,
}

/// Stored interview round record
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterviewRound {
    pub id: i64,
    pub session_id: String,
    pub round_index: i32,
    pub question: String,
    pub transcription: String,
    pub audio_path: String,
    pub sha256: String,
    pub duration_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub file_size_bytes: u64,
    pub created_at: String,
}

/// Stored interview session record
#[derive(Debug, Clone, serde::Serialize)]
pub struct InterviewSession {
    pub id: String,
    pub candidate_name: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub total_rounds: i32,
}

/// Outcome of a round-persistence attempt (RC-4). A COMMIT error is NOT
/// automatically equivalent to "nothing persisted" — an exceptional commit
/// failure (SQLITE_IOERR, SQLITE_FULL, SQLITE_INTERRUPT, SQLITE_NOMEM,
/// filesystem/storage failure) can be ambiguous, so the caller reconciles
/// before deciding evidence fate.
#[derive(Debug, Clone, PartialEq)]
pub enum PersistenceOutcome {
    /// The round row conclusively exists (commit succeeded, or the
    /// reconciliation query proved the committed row matches).
    Committed(i64),
    /// The round is conclusively absent — evidence cleanup may proceed.
    NotCommitted { source: String },
    /// Commit state could not be determined — evidence must NOT be deleted;
    /// quarantine/reconcile instead.
    Unknown { source: String },
}

/// Reject databases created by a NEWER build. A newer schema is not
/// downgradable — silently relabeling it as an older version would corrupt
/// the data model. This check runs before ANY pragma or schema mutation so
/// a future-version database is left completely untouched.
fn ensure_supported_schema_version(conn: &Connection) -> SqlResult<()> {
    let current_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current_version > SCHEMA_VERSION {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "Database schema version {} is newer than supported version {}. Please upgrade the application.",
                current_version, SCHEMA_VERSION
            )),
        ));
    }
    Ok(())
}

impl Database {
    /// Open or create database at the given path
    pub fn open(db_path: &Path) -> SqlResult<Self> {
        let conn = Connection::open(db_path)?;

        // Reject future schema versions before touching anything — the
        // database must remain byte-for-byte untouched (no WAL, no busy
        // timeout, no user_version write) when it is newer than us.
        ensure_supported_schema_version(&conn)?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        // Busy timeout: wait up to 5 seconds for locked database
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;

        // Enforce foreign key constraints
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.initialize()?;
        Ok(db)
    }

    /// Create tables if they don't exist and apply migrations.
    /// Each migration step runs in an explicit transaction so a failure
    /// cannot leave half-migrated tables.
    fn initialize(&self) -> SqlResult<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());

        let current_version: u32 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

        if current_version == 0 {
            // RC-3: RAII transaction — a failed COMMIT triggers a best-effort
            // ROLLBACK in Drop, so the connection is never returned with an
            // unresolved transaction and the original error is surfaced.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    candidate_name TEXT NOT NULL DEFAULT '',
                    started_at TEXT NOT NULL DEFAULT (datetime('now')),
                    completed_at TEXT,
                    total_rounds INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS rounds (
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
            )?;
            tx.commit()?;
        }

        if current_version < 2 {
            // Migrate v1 -> v2: add unique constraint on (session_id, round_index)
            // SQLite doesn't support ALTER TABLE ADD CONSTRAINT, so recreate rounds table.
            // Deterministic tie-breaker: retain the row with the highest primary-key id.
            // Wrapped in an explicit transaction so a failure cannot leave
            // half-migrated tables.
            // RC-3: RAII transaction — a COMMIT failure rolls back in Drop,
            // leaving the connection usable and surfacing the commit error.
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS rounds_new (
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

                INSERT INTO rounds_new
                    (id, session_id, round_index, question, transcription, audio_path,
                     sha256, duration_ms, sample_rate, channels, file_size_bytes, created_at)
                SELECT id, session_id, round_index, question, transcription, audio_path,
                       sha256, duration_ms, sample_rate, channels, file_size_bytes, created_at
                FROM rounds
                WHERE id IN (
                    SELECT MAX(r2.id) FROM rounds r2
                    GROUP BY r2.session_id, r2.round_index
                );

                DROP TABLE rounds;

                ALTER TABLE rounds_new RENAME TO rounds;

                CREATE INDEX IF NOT EXISTS idx_rounds_session ON rounds(session_id);

                -- Reconcile sessions.total_rounds with the LOGICAL round count
                -- that remains after deduplication. v1 could store a stale
                -- counter (e.g. 0) while rounds existed, and duplicate physical
                -- rows collapse into fewer logical rounds — either way the
                -- counter must match the persisted rounds so later rounds
                -- increment from a correct base and a completed session ends
                -- with the right total.
                UPDATE sessions
                SET total_rounds = (
                    SELECT COUNT(*)
                    FROM rounds
                    WHERE rounds.session_id = sessions.id
                );
                ",
            )?;
            tx.commit()?;
        }

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        Ok(())
    }

    /// Execute a closure within a transaction. Rolls back on error.
    ///
    /// RC-3: uses rusqlite's RAII `Transaction`, so EVERY exit path resolves
    /// the transaction: `f` returning Err rolls back explicitly (the original
    /// error stays primary); `f` returning Ok commits, and if COMMIT itself
    /// fails the transaction is rolled back best-effort in Drop — the
    /// connection is never returned with an unresolved transaction.
    pub fn in_transaction<F, R>(&self, f: F) -> SqlResult<R>
    where
        F: FnOnce(&Connection) -> SqlResult<R>,
    {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match f(&tx) {
            Ok(result) => {
                tx.commit()?;
                Ok(result)
            }
            Err(e) => {
                let _ = tx.rollback();
                Err(e)
            }
        }
    }

    /// Create a new interview session
    pub fn create_session(&self, session_id: &str, candidate_name: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO sessions (id, candidate_name) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING",
            params![session_id, candidate_name],
        )?;

        let stored_candidate_name: String = conn.query_row(
            "SELECT candidate_name FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;

        if stored_candidate_name != candidate_name {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
                Some("Session ID already exists for a different candidate".into()),
            ));
        }

        Ok(())
    }

    /// Complete a session. Returns an error if the session does not exist.
    pub fn complete_session(&self, session_id: &str, total_rounds: i32) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let affected = conn.execute(
            "UPDATE sessions SET completed_at = datetime('now'), total_rounds = ?1 WHERE id = ?2",
            params![total_rounds, session_id],
        )?;
        if affected == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Insert a completed interview round
    #[allow(clippy::too_many_arguments)]
    pub fn insert_round(
        &self,
        session_id: &str,
        round_index: i32,
        question: &str,
        transcription: &str,
        audio_path: &str,
        sha256: &str,
        duration_ms: u64,
        sample_rate: u32,
        channels: u16,
        file_size_bytes: u64,
    ) -> SqlResult<i64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO rounds (session_id, round_index, question, transcription, audio_path, sha256, duration_ms, sample_rate, channels, file_size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                round_index,
                question,
                transcription,
                audio_path,
                sha256,
                duration_ms,
                sample_rate,
                channels,
                file_size_bytes,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Insert a round and update the owning session atomically: the round
    /// INSERT, the session total_rounds increment, and — when `is_final` is
    /// true — the session completed_at timestamp all commit in ONE
    /// transaction. Uses plain INSERT — UNIQUE(session_id, round_index)
    /// violation returns NotCommitted so the caller can surface "Round N
    /// already exists". The session update must affect exactly one row (the
    /// session must exist); otherwise the whole transaction rolls back and
    /// no round is persisted.
    ///
    /// RC-4: statement failures (duplicate, missing session) are conclusively
    /// NotCommitted. A COMMIT failure is AMBIGUOUS: the transaction is
    /// rolled back best-effort (the connection stays usable), then the round
    /// is queried by its authoritative identity (session_id + round_index)
    /// and checksum. A matching persisted row means the commit actually went
    /// through (Committed); a conclusively absent row means it did not
    /// (NotCommitted); anything else (query error, mismatched checksum) is
    /// Unknown — the caller must preserve the evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_round_with_session_update(
        &self,
        session_id: &str,
        round_index: i32,
        question: &str,
        transcription: &str,
        audio_path: &str,
        sha256: &str,
        duration_ms: u64,
        sample_rate: u32,
        channels: u16,
        file_size_bytes: u64,
        is_final: bool,
    ) -> PersistenceOutcome {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            Ok(tx) => tx,
            Err(e) => {
                return PersistenceOutcome::Unknown {
                    source: format!("failed to begin persistence transaction: {e}"),
                };
            }
        };
        // Statement phase: any error here is a conclusive rollback — nothing
        // was written.
        let statement_result: SqlResult<i64> = (|| {
            tx.execute(
                "INSERT INTO rounds (session_id, round_index, question, transcription, audio_path, sha256, duration_ms, sample_rate, channels, file_size_bytes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session_id,
                    round_index,
                    question,
                    transcription,
                    audio_path,
                    sha256,
                    duration_ms,
                    sample_rate,
                    channels,
                    file_size_bytes,
                ],
            )?;
            // Session must exist — verify exactly one row is updated. The
            // completed_at timestamp is set in the same statement when this
            // is the final round, so commit is all-or-nothing.
            let affected = tx.execute(
                "UPDATE sessions
                    SET total_rounds = total_rounds + 1,
                        completed_at = CASE WHEN ?2 = 1 THEN datetime('now') ELSE completed_at END
                  WHERE id = ?1",
                params![session_id, if is_final { 1 } else { 0 }],
            )?;
            if affected != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(tx.last_insert_rowid())
        })();

        let id = match statement_result {
            Ok(id) => id,
            Err(e) => {
                // Conclusive: the transaction drops (best-effort rollback)
                // and nothing was committed.
                drop(tx);
                return PersistenceOutcome::NotCommitted {
                    source: e.to_string(),
                };
            }
        };

        match tx.commit() {
            Ok(()) => PersistenceOutcome::Committed(id),
            Err(e) => {
                // RC-4: AMBIGUOUS commit failure. `commit` consumed the
                // transaction (rollback on failure, so the connection stays
                // usable). Reconcile against the authoritative row identity
                // before the caller decides the evidence's fate.
                reconcile_after_commit_failure(&conn, session_id, round_index, sha256, &e)
            }
        }
    }

    /// Whether a round row references the given absolute audio path (RC-1E
    /// evidence reconciliation). Used by startup reconciliation to decide
    /// whether a WAV under recordings/ is durable evidence.
    pub fn round_exists_with_audio_path(&self, audio_path: &str) -> SqlResult<bool> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM rounds WHERE audio_path = ?1",
            params![audio_path],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all rounds for a session
    pub fn get_rounds(&self, session_id: &str) -> SqlResult<Vec<InterviewRound>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, session_id, round_index, question, transcription, audio_path, sha256, duration_ms, sample_rate, channels, file_size_bytes, created_at
             FROM rounds WHERE session_id = ?1 ORDER BY round_index",
        )?;

        let rounds = stmt
            .query_map(params![session_id], |row| {
                Ok(InterviewRound {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    round_index: row.get(2)?,
                    question: row.get(3)?,
                    transcription: row.get(4)?,
                    audio_path: row.get(5)?,
                    sha256: row.get(6)?,
                    duration_ms: row.get(7)?,
                    sample_rate: row.get(8)?,
                    channels: row.get(9)?,
                    file_size_bytes: row.get(10)?,
                    created_at: row.get(11)?,
                })
            })?
            .collect::<SqlResult<Vec<_>>>()?;

        Ok(rounds)
    }

    /// Get all sessions
    pub fn get_sessions(&self) -> SqlResult<Vec<InterviewSession>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, candidate_name, started_at, completed_at, total_rounds FROM sessions ORDER BY started_at DESC",
        )?;

        let sessions = stmt
            .query_map([], |row| {
                Ok(InterviewSession {
                    id: row.get(0)?,
                    candidate_name: row.get(1)?,
                    started_at: row.get(2)?,
                    completed_at: row.get(3)?,
                    total_rounds: row.get(4)?,
                })
            })?
            .collect::<SqlResult<Vec<_>>>()?;

        Ok(sessions)
    }

    /// Get session by ID
    pub fn get_session(&self, session_id: &str) -> SqlResult<Option<InterviewSession>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, candidate_name, started_at, completed_at, total_rounds FROM sessions WHERE id = ?1",
        )?;

        let mut rows = stmt.query_map(params![session_id], |row| {
            Ok(InterviewSession {
                id: row.get(0)?,
                candidate_name: row.get(1)?,
                started_at: row.get(2)?,
                completed_at: row.get(3)?,
                total_rounds: row.get(4)?,
            })
        })?;

        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }
}

/// RC-4: reconcile an AMBIGUOUS COMMIT failure against the authoritative row
/// identity before any evidence decision:
///
/// - row exists AND sha256 matches -> the commit actually went through:
///   `Committed` (evidence retained);
/// - row conclusively absent -> `NotCommitted` (evidence cleanup may
///   proceed);
/// - the reconciliation query itself fails, or the row exists with a
///   mismatched checksum (an anomaly) -> `Unknown` (evidence must NOT be
///   deleted — quarantine/reconcile).
fn reconcile_after_commit_failure(
    conn: &Connection,
    session_id: &str,
    round_index: i32,
    expected_sha256: &str,
    commit_error: &rusqlite::Error,
) -> PersistenceOutcome {
    let row: SqlResult<(i64, String)> = conn.query_row(
        "SELECT id, sha256 FROM rounds WHERE session_id = ?1 AND round_index = ?2",
        params![session_id, round_index],
        |row| Ok((row.get(0)?, row.get(1)?)),
    );
    match row {
        Ok((id, stored_sha)) if stored_sha == expected_sha256 => PersistenceOutcome::Committed(id),
        Ok((_id, stored_sha)) => PersistenceOutcome::Unknown {
            source: format!(
                "COMMIT failed ({commit_error}) and a row exists with a mismatched checksum ({stored_sha:?} != {expected_sha256:?}) — evidence preserved for reconciliation"
            ),
        },
        Err(rusqlite::Error::QueryReturnedNoRows) => PersistenceOutcome::NotCommitted {
            source: format!("COMMIT failed: {commit_error}"),
        },
        Err(e) => PersistenceOutcome::Unknown {
            source: format!(
                "COMMIT failed ({commit_error}) and the reconciliation query failed ({e}) — evidence preserved for reconciliation"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RC-3: a COMMIT that fails (deferred foreign-key violation surfaces at
    /// commit time) must roll the transaction back, leave NO visible row, and
    /// keep the connection fully usable for subsequent operations — the
    /// connection is never returned with an unresolved transaction.
    #[test]
    fn commit_failure_rolls_back_and_keeps_connection_usable() {
        let db_path = std::env::temp_dir().join("test_commit_failure.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        {
            // In-module access: defer FK enforcement to COMMIT time so the
            // INSERT succeeds but COMMIT itself fails.
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute_batch("PRAGMA defer_foreign_keys=ON;").unwrap();
        }

        // The round insert references a session that does not exist; with
        // deferred FKs the INSERT succeeds and COMMIT fails.
        let result = db.in_transaction(|conn| {
            conn.execute(
                "INSERT INTO rounds (session_id, round_index, question) VALUES (?1, ?2, ?3)",
                params!["ghost-session", 0, "Q"],
            )?;
            Ok(())
        });

        assert!(
            result.is_err(),
            "COMMIT must fail on the deferred FK violation"
        );
        assert!(
            db.get_rounds("ghost-session").unwrap().is_empty(),
            "no uncommitted row may become visible after a failed COMMIT"
        );

        // The connection must be fully usable: a real session + round persist
        // normally after the failed commit.
        db.create_session("real-session", "Alice").unwrap();
        let outcome = db.insert_round_with_session_update(
            "real-session",
            0,
            "Q1",
            "A1",
            "/tmp/r0.wav",
            "sha0",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(outcome, PersistenceOutcome::Committed(_)));
        assert_eq!(db.get_rounds("real-session").unwrap().len(), 1);

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-3: an application-statement failure (duplicate round) rolls back the
    /// whole round+session transaction — no partial mutation is visible and
    /// the connection remains usable for the next round.
    #[test]
    fn insert_round_with_session_update_rolls_back_on_error() {
        let db_path = std::env::temp_dir().join("test_round_rollback.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Bob").unwrap();

        let first = db.insert_round_with_session_update(
            "s",
            0,
            "Q1",
            "A1",
            "/tmp/0.wav",
            "sha0",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(first, PersistenceOutcome::Committed(_)));
        assert_eq!(db.get_rounds("s").unwrap().len(), 1);

        // Duplicate (session_id, round_index) -> UNIQUE violation ->
        // conclusively NotCommitted (rolled back).
        let dup = db.insert_round_with_session_update(
            "s",
            0,
            "Q1b",
            "A1b",
            "/tmp/0b.wav",
            "sha1",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(
            matches!(dup, PersistenceOutcome::NotCommitted { .. }),
            "duplicate round must be conclusively NotCommitted"
        );

        // No partial mutation visible: still 1 round, counter not double-
        // counted, original transcription intact.
        let rounds = db.get_rounds("s").unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!(rounds[0].transcription, "A1");
        let session = db.get_session("s").unwrap().unwrap();
        assert_eq!(session.total_rounds, 1);

        // Connection remains usable: the next round persists.
        let next = db.insert_round_with_session_update(
            "s",
            1,
            "Q2",
            "A2",
            "/tmp/1.wav",
            "sha2",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(next, PersistenceOutcome::Committed(_)));
        assert_eq!(db.get_rounds("s").unwrap().len(), 2);

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-3: a round whose session update affects zero rows (missing session)
    /// fails and rolls back — a round can never be persisted against a
    /// session that does not exist.
    #[test]
    fn insert_round_for_missing_session_rolls_back() {
        let db_path = std::env::temp_dir().join("test_missing_session_rollback.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        let result = db.insert_round_with_session_update(
            "ghost",
            0,
            "Q",
            "A",
            "/tmp/x.wav",
            "sha",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(
            matches!(result, PersistenceOutcome::NotCommitted { .. }),
            "missing session must be conclusively NotCommitted"
        );
        assert!(db.get_rounds("ghost").unwrap().is_empty());

        // Connection remains usable.
        db.create_session("real", "Carol").unwrap();
        let ok = db.insert_round_with_session_update(
            "real",
            0,
            "Q",
            "A",
            "/tmp/x.wav",
            "sha",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(ok, PersistenceOutcome::Committed(_)));
        assert_eq!(db.get_rounds("real").unwrap().len(), 1);

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-4: a normal commit succeeds -> Committed (evidence retained).
    #[test]
    fn insert_round_normal_commit_is_committed() {
        let db_path = std::env::temp_dir().join("test_normal_commit.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Dave").unwrap();
        let outcome = db.insert_round_with_session_update(
            "s",
            0,
            "Q",
            "A",
            "/tmp/r0.wav",
            "sha0",
            4000,
            16000,
            1,
            100,
            true,
        );
        assert!(matches!(outcome, PersistenceOutcome::Committed(id) if id > 0));
        assert_eq!(db.get_rounds("s").unwrap().len(), 1);

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-4: after an ambiguous COMMIT failure, a matching persisted row
    /// (same authoritative identity AND checksum) means the commit actually
    /// went through -> Committed, so the evidence must be retained.
    #[test]
    fn reconcile_after_commit_failure_matching_row_is_committed() {
        let db_path = std::env::temp_dir().join("test_reconcile_match.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Eve").unwrap();
        // Simulate a commit that actually landed at the storage level: the
        // row is present with the expected checksum.
        let outcome = db.insert_round_with_session_update(
            "s",
            0,
            "Q",
            "A",
            "/tmp/r0.wav",
            "sha0",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(outcome, PersistenceOutcome::Committed(_)));

        // Now simulate the ambiguous-commit path with a matching row: the
        // reconciler must conclude Committed, not delete evidence.
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let synthetic_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
            Some("simulated storage error".into()),
        );
        let reconciled = reconcile_after_commit_failure(&conn, "s", 0, "sha0", &synthetic_error);
        assert!(
            matches!(reconciled, PersistenceOutcome::Committed(_)),
            "matching row + checksum must be treated as committed"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-4: after an ambiguous COMMIT failure with NO persisted row, the
    /// round is conclusively absent -> NotCommitted (evidence cleanup may
    /// proceed).
    #[test]
    fn reconcile_after_commit_failure_no_row_is_not_committed() {
        let db_path = std::env::temp_dir().join("test_reconcile_absent.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Frank").unwrap();
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let synthetic_error = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            Some("simulated full disk".into()),
        );
        let reconciled = reconcile_after_commit_failure(&conn, "s", 0, "sha0", &synthetic_error);
        assert!(
            matches!(reconciled, PersistenceOutcome::NotCommitted { .. }),
            "no row must be conclusively NotCommitted"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-4: when the reconciliation query itself fails (broken table), the
    /// outcome is Unknown — the evidence must NOT be deleted blindly.
    #[test]
    fn reconcile_after_commit_failure_query_error_is_unknown() {
        let db_path = std::env::temp_dir().join("test_reconcile_unknown.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Grace").unwrap();
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            // Break the rounds table so the reconciliation query errors.
            conn.execute_batch("DROP TABLE rounds;").unwrap();
            let synthetic_error = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                Some("simulated storage error".into()),
            );
            let reconciled =
                reconcile_after_commit_failure(&conn, "s", 0, "sha0", &synthetic_error);
            assert!(
                matches!(reconciled, PersistenceOutcome::Unknown { .. }),
                "an unresolvable reconciliation must be Unknown, never a false NotCommitted"
            );
        }

        let _ = std::fs::remove_file(&db_path);
    }

    /// RC-1E: round_exists_with_audio_path distinguishes durable evidence
    /// from orphaned WAVs.
    #[test]
    fn round_exists_with_audio_path_reconciles_evidence() {
        let db_path = std::env::temp_dir().join("test_evidence_reconcile.db");
        let _ = std::fs::remove_file(&db_path);

        let db = Database::open(&db_path).unwrap();
        db.create_session("s", "Heidi").unwrap();
        assert!(!db.round_exists_with_audio_path("/tmp/never.wav").unwrap());

        let outcome = db.insert_round_with_session_update(
            "s",
            0,
            "Q",
            "A",
            "/tmp/r0.wav",
            "sha0",
            4000,
            16000,
            1,
            100,
            false,
        );
        assert!(matches!(outcome, PersistenceOutcome::Committed(_)));
        assert!(db.round_exists_with_audio_path("/tmp/r0.wav").unwrap());
        assert!(!db.round_exists_with_audio_path("/tmp/other.wav").unwrap());

        let _ = std::fs::remove_file(&db_path);
    }
}
