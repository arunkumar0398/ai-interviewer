use rusqlite::{params, Connection, Result as SqlResult};
use std::path::Path;
use std::sync::Mutex;

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

impl Database {
    /// Open or create database at the given path
    pub fn open(db_path: &Path) -> SqlResult<Self> {
        let conn = Connection::open(db_path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.initialize()?;
        Ok(db)
    }

    /// Create tables if they don't exist
    fn initialize(&self) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch(
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
                FOREIGN KEY (session_id) REFERENCES sessions(id)
            );

            CREATE INDEX IF NOT EXISTS idx_rounds_session ON rounds(session_id);
            ",
        )?;

        Ok(())
    }

    /// Create a new interview session
    pub fn create_session(&self, session_id: &str, candidate_name: &str) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, candidate_name) VALUES (?1, ?2)",
            params![session_id, candidate_name],
        )?;
        Ok(())
    }

    /// Complete a session
    pub fn complete_session(&self, session_id: &str, total_rounds: i32) -> SqlResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET completed_at = datetime('now'), total_rounds = ?1 WHERE id = ?2",
            params![total_rounds, session_id],
        )?;
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
        let conn = self.conn.lock().unwrap();
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

    /// Get all rounds for a session
    pub fn get_rounds(&self, session_id: &str) -> SqlResult<Vec<InterviewRound>> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
