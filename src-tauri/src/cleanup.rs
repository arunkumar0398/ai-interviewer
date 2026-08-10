//! Controlled artifact cleanup (RC-1).
//!
//! Sensitive candidate/test/transcript artifacts must NEVER be deleted with a
//! swallowed `let _ = std::fs::remove_file(...)` while the code then claims
//! cleanup succeeded. On Windows, deletion can fail with a sharing violation,
//! AccessDenied, a transient antivirus/indexer lock, or an I/O error — a
//! "cleanup succeeded" claim must be backed by an actually successful
//! deletion.
//!
//! This module provides:
//! - `CleanupOutcome`: an observable result for a controlled deletion.
//! - `remove_owned`: the single controlled deletion entry point.
//! - `reconcile_stale_artifacts`: startup reconciliation that removes
//!   PROVABLY OWNED stale temporary artifacts (never candidate evidence) and
//!   reports everything else.
//!
//! Ownership is never inferred from a filename alone: callers only invoke
//! `remove_owned` for paths they provably created (invocation-unique temp
//! paths, guard-tracked provisional WAVs, or artifacts with known generated
//! prefixes). Drop-time best-effort deletion stays only as defense-in-depth
//! behind this explicit controlled path.

use std::path::Path;

/// Result of a controlled deletion attempt (RC-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// The artifact was removed.
    Removed,
    /// The artifact was already absent (no error — nothing to do).
    AlreadyAbsent,
    /// Removal failed with a transient kind (permission/sharing violation,
    /// lock, timeout) — retry/reconciliation is possible.
    RetryableFailure(std::io::ErrorKind),
    /// Removal failed with a non-transient kind — reconciliation required.
    PermanentFailure(std::io::ErrorKind),
}

impl CleanupOutcome {
    /// Whether the artifact is confirmed gone.
    pub fn succeeded(&self) -> bool {
        matches!(
            self,
            CleanupOutcome::Removed | CleanupOutcome::AlreadyAbsent
        )
    }

    /// Whether the artifact may still exist.
    pub fn failed(&self) -> bool {
        !self.succeeded()
    }
}

/// Windows-relevant transient failure kinds: a sharing violation surfaces as
/// PermissionDenied; a lock held by an indexer/antivirus often surfaces as
/// WouldBlock; interrupted/timeout I/O is also retryable.
pub fn is_retryable(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
    )
}

/// Controlled deletion of an owned artifact (RC-1). `NotFound` is treated as
/// success (`AlreadyAbsent`). Any other failure is classified as retryable or
/// permanent so the caller can decide whether to retry/reconcile.
pub fn remove_owned(path: &Path) -> CleanupOutcome {
    match std::fs::remove_file(path) {
        Ok(()) => CleanupOutcome::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CleanupOutcome::AlreadyAbsent,
        Err(e) if is_retryable(e.kind()) => CleanupOutcome::RetryableFailure(e.kind()),
        Err(e) => CleanupOutcome::PermanentFailure(e.kind()),
    }
}

/// Human-readable description of a cleanup outcome, including the path, for
/// surfacing in errors/logs without claiming success.
pub fn describe(outcome: &CleanupOutcome, path: &Path) -> String {
    match outcome {
        CleanupOutcome::Removed => format!("removed {}", path.display()),
        CleanupOutcome::AlreadyAbsent => format!("already absent: {}", path.display()),
        CleanupOutcome::RetryableFailure(kind) => {
            format!("transient failure removing {} ({:?})", path.display(), kind)
        }
        CleanupOutcome::PermanentFailure(kind) => {
            format!("permanent failure removing {} ({:?})", path.display(), kind)
        }
    }
}

/// Startup reconciliation (RC-1E). Removes PROVABLY OWNED stale temporary
/// artifacts that a crashed/timed-out run could have left behind:
///
/// - `temp/**/audio_test_*.wav` and `*.wav.tmp` (known generated prefix);
/// - `temp/**/device_test_*.wav` and `*.wav.tmp` (known generated prefix);
/// - `temp/<session>/*.txt` (transcripts are NEVER the durable form — only
///   their text is persisted, so any `.txt` under the session temp dir is
///   provably a leftover temp);
/// - `recordings/**/*.wav.tmp` (a `.wav.tmp` is by definition an uncommitted
///   partial — it can never be durable evidence);
/// - `tts_dir/*.<uuid>.wav.tmp` (invocation-unique standalone-TTS temps).
///
/// Candidate FINAL WAVs under `recordings/` are NEVER deleted here — they are
/// reconciled against the database by `reconcile_orphaned_evidence`.
///
/// Returns a list of human-readable reconciliation messages for logging.
pub fn reconcile_stale_artifacts(paths: &crate::paths::AppPaths) -> Vec<String> {
    let mut messages = Vec::new();
    let mut removed = 0usize;

    // 1. audio_test / device_test artifacts under the temp dir (any depth).
    if let Ok(entries) = std::fs::read_dir(&paths.temp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                let is_test_artifact = (name.starts_with("audio_test_")
                    || name.starts_with("device_test_"))
                    && (name.ends_with(".wav") || name.ends_with(".wav.tmp"));
                if is_test_artifact {
                    if remove_owned(&path).succeeded() {
                        removed += 1;
                    } else {
                        messages.push(format!(
                            "[startup-reconcile] could not remove stale test artifact: {}",
                            path.display()
                        ));
                    }
                }
            }
        }
    }

    // 2. Session transcript temps: temp/<session>/*.txt (legacy deterministic
    //    and invocation-unique forms). Transcripts are never durable.
    if let Ok(sessions) = std::fs::read_dir(&paths.temp_dir) {
        for session in sessions.flatten() {
            let session_dir = session.path();
            if !session_dir.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&session_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.is_file() && path.extension().map(|e| e == "txt").unwrap_or(false) {
                        if remove_owned(&path).succeeded() {
                            removed += 1;
                        } else {
                            messages.push(format!(
                                "[startup-reconcile] could not remove stale transcript temp: {}",
                                path.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    // 3. Partial round WAVs: recordings/**/*.wav.tmp (never durable evidence).
    if let Ok(sessions) = std::fs::read_dir(&paths.recordings_dir) {
        for session in sessions.flatten() {
            let session_dir = session.path();
            if !session_dir.is_dir() {
                continue;
            }
            if let Ok(files) = std::fs::read_dir(&session_dir) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.is_file()
                        && path
                            .file_name()
                            .map(|n| n.to_string_lossy().ends_with(".wav.tmp"))
                            .unwrap_or(false)
                    {
                        if remove_owned(&path).succeeded() {
                            removed += 1;
                        } else {
                            messages.push(format!(
                                "[startup-reconcile] could not remove stale partial WAV: {}",
                                path.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    // 4. Standalone-TTS invocation temps: tts_dir/*.<uuid>.wav.tmp. The
    //    invocation-unique pattern is `<stem>.<uuid>.wav.tmp` — the stem is
    //    a UUID request id followed by the invocation UUID, so any `.wav.tmp`
    //    directly under tts_dir is provably a leftover temp.
    if let Ok(entries) = std::fs::read_dir(&paths.tts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .map(|n| n.to_string_lossy().ends_with(".wav.tmp"))
                    .unwrap_or(false)
            {
                if remove_owned(&path).succeeded() {
                    removed += 1;
                } else {
                    messages.push(format!(
                        "[startup-reconcile] could not remove stale TTS temp: {}",
                        path.display()
                    ));
                }
            }
        }
    }

    if removed > 0 {
        messages.push(format!(
            "[startup-reconcile] removed {removed} stale temporary artifact(s)"
        ));
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_owned_reports_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.wav");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(remove_owned(&path), CleanupOutcome::Removed);
        assert!(!path.exists());
    }

    #[test]
    fn remove_owned_reports_already_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.wav");
        assert_eq!(remove_owned(&path), CleanupOutcome::AlreadyAbsent);
    }

    #[test]
    fn remove_owned_classifies_non_file_failure() {
        // `remove_file` on a DIRECTORY fails on every platform (AccessDenied
        // on Windows, IsADirectory on Unix) — the failure is surfaced, never
        // converted to success.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a-directory");
        std::fs::create_dir_all(&target).unwrap();
        let outcome = remove_owned(&target);
        assert!(outcome.failed(), "expected a failure, got {:?}", outcome);
    }

    #[test]
    fn reconcile_removes_stale_test_and_transcript_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            tmp.path().join("tools"),
            tmp.path().join("data"),
        )
        .unwrap();
        std::fs::create_dir_all(&paths.temp_dir).unwrap();
        std::fs::create_dir_all(&paths.recordings_dir).unwrap();
        std::fs::create_dir_all(&paths.tts_dir).unwrap();

        let stale_test = paths.temp_dir.join("audio_test_abc.wav");
        std::fs::write(&stale_test, b"x").unwrap();
        let stale_device_tmp = paths.temp_dir.join("device_test_xyz.wav.tmp");
        std::fs::write(&stale_device_tmp, b"x").unwrap();
        let session_dir = paths.temp_dir.join("session-123");
        std::fs::create_dir_all(&session_dir).unwrap();
        let stale_transcript = session_dir.join("round.inv.txt");
        std::fs::write(&stale_transcript, b"x").unwrap();
        let legacy_transcript = session_dir.join("round.txt");
        std::fs::write(&legacy_transcript, b"x").unwrap();
        let session_rec = paths.recordings_dir.join("session-123");
        std::fs::create_dir_all(&session_rec).unwrap();
        let stale_partial = session_rec.join("round.wav.tmp");
        std::fs::write(&stale_partial, b"x").unwrap();
        let stale_tts = paths.tts_dir.join("req.inv.wav.tmp");
        std::fs::write(&stale_tts, b"x").unwrap();

        reconcile_stale_artifacts(&paths);

        assert!(!stale_test.exists());
        assert!(!stale_device_tmp.exists());
        assert!(!stale_transcript.exists());
        assert!(!legacy_transcript.exists());
        assert!(!stale_partial.exists());
        assert!(!stale_tts.exists());
    }

    #[test]
    fn reconcile_never_deletes_persisted_round_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::AppPaths::from_tool_dir(
            tmp.path().join("tools"),
            tmp.path().join("data"),
        )
        .unwrap();
        let session_dir = paths.recordings_dir.join("session-123");
        std::fs::create_dir_all(&session_dir).unwrap();
        let committed = session_dir.join("round.wav");
        std::fs::write(&committed, b"committed-evidence").unwrap();

        reconcile_stale_artifacts(&paths);

        assert!(committed.exists(), "committed WAV must never be deleted");
        assert_eq!(std::fs::read(&committed).unwrap(), b"committed-evidence");
    }
}
