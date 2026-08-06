use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// Error type for database path resolution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DatabasePathError {
    /// Legacy database exists but could not be renamed to canonical path.
    MigrationFailed {
        legacy: String,
        canonical: String,
        error: String,
    },
}

impl std::fmt::Display for DatabasePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MigrationFailed {
                legacy,
                canonical,
                error,
            } => write!(
                f,
                "Failed to migrate database from {legacy} to {canonical}: {error}"
            ),
        }
    }
}

impl std::error::Error for DatabasePathError {}

/// Describes how the tool directory was resolved.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolDirectorySource {
    /// User set `AI_INTERVIEWER_TOOLS` environment variable.
    EnvVar { value: String },
    /// Tauri bundled resources directory (production install).
    Bundled,
    /// Portable layout next to the executable.
    Portable { exe_dir: String },
    /// Development fallback — not available in release builds.
    DevFallback,
    /// Could not locate a tools directory.
    Unresolved,
}

/// A single configuration issue surfaced to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfigurationIssue {
    pub code: String,
    pub message: String,
    pub expected_path: Option<String>,
}

/// Aggregate readiness state returned alongside `AppConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppReadiness {
    pub ready: bool,
    pub issues: Vec<AppConfigurationIssue>,
}

/// Fully-resolved paths for the application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppPaths {
    pub tool_dir: PathBuf,
    pub db_path: PathBuf,
    pub recordings_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub is_portable: bool,
    pub tool_directory_source: ToolDirectorySource,
}

/// Compact config sent to the frontend via `get_app_config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub tool_dir: String,
    pub db_path: String,
    pub recordings_dir: String,
    pub temp_dir: String,
    pub is_portable: bool,
    pub tool_directory_source: ToolDirectorySource,
    pub readiness: AppReadiness,
}

/// Input for testable path resolution. Allows injecting directories without
/// relying on env vars or exe location.
#[derive(Debug, Clone)]
pub struct PathResolutionInput {
    pub exe_dir: PathBuf,
    pub app_data_dir: PathBuf,
}

impl AppPaths {
    /// Build paths from an explicit tool directory (used by tests and the
    /// audio spike binary).
    pub fn from_tool_dir(tool_dir: PathBuf, data_dir: PathBuf) -> Self {
        let recordings_dir = data_dir.join("recordings");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path_legacy(&data_dir);
        let tool_directory_source = ToolDirectorySource::DevFallback;

        Self {
            tool_dir,
            db_path,
            recordings_dir,
            temp_dir,
            is_portable: false,
            tool_directory_source,
        }
    }

    /// Resolve paths from injected inputs (testable without env vars).
    pub fn resolve_from_input(input: PathResolutionInput) -> Self {
        let (tool_dir, tool_directory_source, is_portable) =
            resolve_tool_dir(&input.exe_dir);

        let data_dir = if is_portable {
            input.exe_dir.clone()
        } else {
            input.app_data_dir.clone()
        };

        let recordings_dir = data_dir.join("recordings");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path(&data_dir).unwrap_or_else(|e| {
            // Fatal: log and panic — caller must handle this before construction.
            panic!("Database path resolution failed: {e}");
        });

        Self {
            tool_dir,
            db_path,
            recordings_dir,
            temp_dir,
            is_portable,
            tool_directory_source,
        }
    }

    /// Canonical constructor used at application startup.
    pub fn resolve() -> Self {
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let (tool_dir, tool_directory_source, is_portable) = resolve_tool_dir(&exe_dir);

        let data_dir = compute_data_dir(&exe_dir, is_portable);
        let recordings_dir = data_dir.join("recordings");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path(&data_dir).unwrap_or_else(|e| {
            panic!("Database path resolution failed: {e}");
        });

        Self {
            tool_dir,
            db_path,
            recordings_dir,
            temp_dir,
            is_portable,
            tool_directory_source,
        }
    }

    /// Produce the compact config sent to the frontend.
    pub fn to_app_config(&self) -> AppConfig {
        let readiness = self.validate_readiness();
        AppConfig {
            tool_dir: self.tool_dir.display().to_string(),
            db_path: self.db_path.display().to_string(),
            recordings_dir: self.recordings_dir.display().to_string(),
            temp_dir: self.temp_dir.display().to_string(),
            is_portable: self.is_portable,
            tool_directory_source: self.tool_directory_source.clone(),
            readiness,
        }
    }

    /// Ensure required directories exist.
    pub fn ensure_directories(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.recordings_dir)
            .map_err(|e| format!("Failed to create recordings dir: {e}"))?;
        std::fs::create_dir_all(&self.temp_dir)
            .map_err(|e| format!("Failed to create temp dir: {e}"))?;
        Ok(())
    }

    /// Validate that critical binaries and models are present. Returns the
    /// aggregate readiness state — callers decide whether to surface errors
    /// or degrade gracefully.
    pub fn validate_readiness(&self) -> AppReadiness {
        let mut issues = Vec::new();

        // Canonical layout: tools/piper/piper.exe  (legacy: tools/piper/piper/piper.exe)
        let piper_canonical = self.tool_dir.join("piper").join("piper.exe");
        let piper_legacy = self.tool_dir.join("piper").join("piper").join("piper.exe");
        if !piper_canonical.exists() && !piper_legacy.exists() {
            issues.push(AppConfigurationIssue {
                code: "PIPER_BINARY_MISSING".into(),
                message: "Piper TTS binary not found".into(),
                expected_path: Some(piper_canonical.display().to_string()),
            });
        }

        // Whisper binary: tools/whisper/Release/main.exe
        let whisper_path = self
            .tool_dir
            .join("whisper")
            .join("Release")
            .join("main.exe");
        if !whisper_path.exists() {
            issues.push(AppConfigurationIssue {
                code: "WHISPER_BINARY_MISSING".into(),
                message: "Whisper binary not found".into(),
                expected_path: Some(whisper_path.display().to_string()),
            });
        }

        // Piper model: tools/piper/model.onnx (legacy: tools/piper-models/en_US-amy-medium.onnx)
        let model_canonical = self.tool_dir.join("piper").join("model.onnx");
        let model_legacy = self
            .tool_dir
            .join("piper-models")
            .join("en_US-amy-medium.onnx");
        if !model_canonical.exists() && !model_legacy.exists() {
            issues.push(AppConfigurationIssue {
                code: "PIPER_MODEL_MISSING".into(),
                message: "Piper model not found".into(),
                expected_path: Some(model_canonical.display().to_string()),
            });
        }

        // Whisper model: tools/models/ggml-tiny.en.bin
        let whisper_model = self.tool_dir.join("models").join("ggml-tiny.en.bin");
        if !whisper_model.exists() {
            issues.push(AppConfigurationIssue {
                code: "WHISPER_MODEL_MISSING".into(),
                message: "Whisper model not found".into(),
                expected_path: Some(whisper_model.display().to_string()),
            });
        }

        AppReadiness {
            ready: issues.is_empty(),
            issues,
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution helpers (module-level so they can be used by tests and the
// audio spike binary without going through AppPaths).
// ---------------------------------------------------------------------------

/// Determine the tools directory and how it was found.
///
/// Resolution order:
/// 1. `AI_INTERVIEWER_TOOLS` env var
/// 2. Tauri bundled resources (production)
/// 3. Portable layout next to the exe
/// 4. Dev fallback (debug builds only)
pub fn resolve_tool_dir(exe_dir: &Path) -> (PathBuf, ToolDirectorySource, bool) {
    // 1. Explicit env var
    if let Ok(val) = env::var("AI_INTERVIEWER_TOOLS") {
        let p = PathBuf::from(&val);
        if p.exists() {
            return (p, ToolDirectorySource::EnvVar { value: val }, false);
        }
    }

    // 2. Tauri bundled resources — on Windows the bundle root sits one level
    //    above the exe directory.
    let bundled = exe_dir.join("resources").join("tools");
    if bundled.exists() {
        return (bundled, ToolDirectorySource::Bundled, false);
    }

    // 3. Portable layout next to the exe
    let portable = exe_dir.join("tools");
    if portable.exists() {
        return (
            portable.clone(),
            ToolDirectorySource::Portable {
                exe_dir: exe_dir.display().to_string(),
            },
            true,
        );
    }

    // 4. Dev fallback — only in debug builds, resolves relative to workspace
    #[cfg(debug_assertions)]
    {
        let dev = dev_tools_dir();
        if dev.exists() {
            return (dev, ToolDirectorySource::DevFallback, false);
        }
    }

    // Nothing found — return the exe_dir/tools path as a best-effort default
    // so the caller gets a valid PathBuf while readiness flags the issue.
    (
        exe_dir.join("tools"),
        ToolDirectorySource::Unresolved,
        false,
    )
}

/// Canonical data directory next to the exe (portable) or in %LOCALAPPDATA%.
fn compute_data_dir(exe_dir: &Path, is_portable: bool) -> PathBuf {
    if is_portable {
        return exe_dir.to_path_buf();
    }
    if let Ok(local) = env::var("LOCALAPPDATA") {
        return PathBuf::from(local).join("ai-interviewer");
    }
    exe_dir.to_path_buf()
}

/// Resolve the database file path.  Prefers `interviews.db` (canonical).
/// When the legacy `interviewer.db` exists but canonical does not, attempts
/// to rename (move) the legacy file to the canonical path.
///
/// Returns `Err(DatabasePathError::MigrationFailed)` if the rename fails.
fn resolve_database_path(data_dir: &Path) -> Result<PathBuf, DatabasePathError> {
    let canonical = data_dir.join("interviews.db");
    let legacy = data_dir.join("interviewer.db");

    if canonical.exists() {
        return Ok(canonical);
    }
    if legacy.exists() {
        match std::fs::rename(&legacy, &canonical) {
            Ok(()) => return Ok(canonical),
            Err(e) => {
                return Err(DatabasePathError::MigrationFailed {
                    legacy: legacy.display().to_string(),
                    canonical: canonical.display().to_string(),
                    error: e.to_string(),
                });
            }
        }
    }
    // Neither exists — use canonical name for new databases.
    Ok(canonical)
}

/// Legacy version of `resolve_database_path` that silently falls back.
/// Used only by `from_tool_dir` for backward-compatible test construction.
fn resolve_database_path_legacy(data_dir: &Path) -> PathBuf {
    resolve_database_path(data_dir).unwrap_or_else(|_| data_dir.join("interviews.db"))
}

/// Resolve Piper binary and model paths, supporting both canonical and legacy
/// layouts.  Returns `None` for any component that is not found.
pub fn resolve_piper_paths(tool_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    // Binary
    let bin_canonical = tool_dir.join("piper").join("piper.exe");
    let bin_legacy = tool_dir.join("piper").join("piper").join("piper.exe");
    let bin = if bin_canonical.exists() {
        Some(bin_canonical)
    } else if bin_legacy.exists() {
        Some(bin_legacy)
    } else {
        None
    };

    // Model
    let model_canonical = tool_dir.join("piper").join("model.onnx");
    let model_legacy = tool_dir.join("piper-models").join("en_US-amy-medium.onnx");
    let model = if model_canonical.exists() {
        Some(model_canonical)
    } else if model_legacy.exists() {
        Some(model_legacy)
    } else {
        None
    };

    (bin, model)
}

/// Resolve Whisper binary path.
pub fn resolve_whisper_path(tool_dir: &Path) -> Option<PathBuf> {
    let p = tool_dir.join("whisper").join("Release").join("main.exe");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Resolve Whisper model path.
pub fn resolve_whisper_model_path(tool_dir: &Path) -> Option<PathBuf> {
    let p = tool_dir.join("models").join("ggml-tiny.en.bin");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Path to the project's `ai-interviewer-tools` directory.
///
/// Resolution order:
/// 1. `AI_INTERVIEWER_TOOLS` env var
/// 2. `tools` directory next to the executable
/// 3. `tools` directory next to the workspace Cargo.toml (dev only)
pub fn resolve_tools_path() -> Option<PathBuf> {
    // 1. Env var
    if let Ok(val) = env::var("AI_INTERVIEWER_TOOLS") {
        let p = PathBuf::from(&val);
        if p.exists() {
            return Some(p);
        }
    }

    // 2. Next to the executable
    if let Ok(exe) = env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let p = exe_dir.join("tools");
            if p.exists() {
                return Some(p);
            }
        }
    }

    // 3. Dev fallback — next to workspace Cargo.toml
    #[cfg(debug_assertions)]
    {
        let dev = dev_tools_dir();
        if dev.exists() {
            return Some(dev);
        }
    }

    None
}

/// Dev-only: resolve `tools` directory from the workspace root.
#[cfg(debug_assertions)]
fn dev_tools_dir() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .parent()
        .expect("workspace root")
        .join("tools")
}

// ---------------------------------------------------------------------------
// Tauri integration
// ---------------------------------------------------------------------------

/// Shared state managed by Tauri, holds the resolved `AppPaths`.
pub struct PathsState {
    pub paths: AppPaths,
}

/// Resolve paths at application startup.
///
/// `app_data_dir()` is the authoritative storage root — failure is fatal.
pub fn resolve_app_paths(app: &tauri::AppHandle) -> Result<PathsState, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Tauri app_data_dir() failed (fatal): {e}"))?;

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let input = PathResolutionInput {
        exe_dir,
        app_data_dir,
    };
    let app_paths = AppPaths::resolve_from_input(input);
    app_paths.ensure_directories()?;
    Ok(PathsState { paths: app_paths })
}

/// Tauri command: return compact config (with readiness) to the frontend.
#[tauri::command]
pub fn get_app_config(paths: tauri::State<'_, PathsState>) -> AppConfig {
    paths.paths.to_app_config()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn from_tool_dir_sets_db_in_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        fs::create_dir_all(&data).unwrap();

        let paths = AppPaths::from_tool_dir(tool.clone(), data.clone());
        assert_eq!(paths.tool_dir, tool);
        assert_eq!(paths.db_path, data.join("interviews.db"));
        assert_eq!(paths.recordings_dir, data.join("recordings"));
        assert_eq!(paths.temp_dir, data.join("temp"));
        assert!(!paths.is_portable);
    }

    #[test]
    fn to_app_config_includes_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        fs::create_dir_all(&data).unwrap();

        let paths = AppPaths::from_tool_dir(tool, data);
        let config = paths.to_app_config();
        assert!(!config.readiness.ready);
        assert!(!config.readiness.issues.is_empty());
        // Every issue has a code and message
        for issue in &config.readiness.issues {
            assert!(!issue.code.is_empty());
            assert!(!issue.message.is_empty());
        }
    }

    #[test]
    fn validate_readiness_passes_when_all_present() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Create canonical layout
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

    #[test]
    fn validate_readiness_fails_when_piper_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Everything except piper
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data"));
        let readiness = paths.validate_readiness();
        assert!(!readiness.ready);
        assert!(readiness
            .issues
            .iter()
            .any(|i| i.code == "PIPER_BINARY_MISSING"));
    }

    #[test]
    fn resolve_database_path_prefers_canonical() {
        let tmp = tempfile::tempdir().unwrap();

        // Both exist — canonical wins
        fs::write(tmp.path().join("interviews.db"), b"").unwrap();
        fs::write(tmp.path().join("interviewer.db"), b"").unwrap();
        assert_eq!(
            resolve_database_path(tmp.path()).unwrap(),
            tmp.path().join("interviews.db")
        );
    }

    #[test]
    fn resolve_database_path_migrates_legacy() {
        let tmp = tempfile::tempdir().unwrap();

        // Only legacy exists — should rename to canonical
        fs::write(tmp.path().join("interviewer.db"), b"legacy").unwrap();
        let result = resolve_database_path(tmp.path());
        assert_eq!(result.unwrap(), tmp.path().join("interviews.db"));
        // Legacy should no longer exist after migration
        assert!(!tmp.path().join("interviewer.db").exists());
    }

    #[test]
    fn resolve_database_path_uses_canonical_when_neither_exists() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_database_path(tmp.path()).unwrap(),
            tmp.path().join("interviews.db")
        );
    }

    #[test]
    fn resolve_piper_paths_canonical_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
        fs::write(tool.join("piper").join("model.onnx"), b"").unwrap();

        let (bin, model) = resolve_piper_paths(&tool);
        assert_eq!(bin, Some(tool.join("piper").join("piper.exe")));
        assert_eq!(model, Some(tool.join("piper").join("model.onnx")));
    }

    #[test]
    fn resolve_piper_paths_legacy_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        fs::create_dir_all(tool.join("piper").join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper").join("piper.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("piper-models")).unwrap();
        fs::write(tool.join("piper-models").join("en_US-amy-medium.onnx"), b"").unwrap();

        let (bin, model) = resolve_piper_paths(&tool);
        assert_eq!(
            bin,
            Some(tool.join("piper").join("piper").join("piper.exe"))
        );
        assert_eq!(
            model,
            Some(tool.join("piper-models").join("en_US-amy-medium.onnx"))
        );
    }

    #[test]
    fn resolve_piper_paths_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        fs::create_dir_all(&tool).unwrap();

        let (bin, model) = resolve_piper_paths(&tool);
        assert_eq!(bin, None);
        assert_eq!(model, None);
    }

    #[test]
    fn resolve_whisper_path_found() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();

        assert_eq!(
            resolve_whisper_path(&tool),
            Some(tool.join("whisper").join("Release").join("main.exe"))
        );
    }

    #[test]
    fn resolve_whisper_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        fs::create_dir_all(&tool).unwrap();

        assert_eq!(resolve_whisper_path(&tool), None);
    }

    #[test]
    fn ensure_directories_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();

        let paths = AppPaths::from_tool_dir(tool, data);
        paths.ensure_directories().unwrap();
        assert!(paths.recordings_dir.exists());
        assert!(paths.temp_dir.exists());
    }
}
