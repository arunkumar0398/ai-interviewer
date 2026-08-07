use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use tauri::Manager;
use uuid::Uuid;

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
    pub tts_dir: PathBuf,
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
    pub tts_dir: String,
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
    /// Tauri resource directory (optional override for tool distribution).
    pub resource_dir: Option<PathBuf>,
}

/// Describes the distribution mode for external tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolDistributionMode {
    /// Tools bundled via Tauri resource bundling.
    Bundled,
    /// Portable layout — tools live next to the executable.
    Portable,
    /// Resolved from the `AI_INTERVIEWER_TOOLS` env var.
    EnvVar { value: String },
    /// Dev fallback — only in debug builds.
    DevFallback,
    /// Could not resolve a tools directory.
    Unresolved,
}

/// Options for testable tool directory resolution. Replaces `set_var` in tests.
#[derive(Debug, Clone, Default)]
pub struct ToolDirOptions {
    /// Override the env var value. `None` means do not check env var.
    pub env_override: Option<String>,
    /// Tauri resource directory (from `resource_dir()`).
    pub resource_dir: Option<PathBuf>,
    /// Whether to allow the dev fallback (default: false in tests).
    pub allow_dev_fallback: bool,
}

/// Validate that an identifier is safe for use as a directory or file name
/// component. This is a lighter check than `validate_uuid` for contexts
/// where UUID format is not required but traversal must still be blocked.
pub fn validate_path_component(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("Identifier must not be empty".into());
    }
    if id == "." || id == ".." {
        return Err("Identifier must not be '.' or '..'".into());
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') {
        return Err("Identifier contains invalid path characters".into());
    }
    Ok(())
}

/// Convert a UUID to a hyphenated lowercase string safe for use as a directory name.
pub fn uuid_to_path(id: &Uuid) -> String {
    id.hyphenated().to_string()
}

impl AppPaths {
    /// Return the recordings directory for a specific session.
    pub fn session_recordings_dir(&self, session_id: Uuid) -> PathBuf {
        self.recordings_dir.join(uuid_to_path(&session_id))
    }

    /// Return the temp directory for a specific session.
    pub fn session_temp_dir(&self, session_id: Uuid) -> PathBuf {
        self.temp_dir.join(uuid_to_path(&session_id))
    }

    /// Return the WAV path for a specific round within a session.
    pub fn round_audio_path(&self, session_id: Uuid, round_id: Uuid) -> PathBuf {
        self.session_recordings_dir(session_id)
            .join(format!("{}.wav", uuid_to_path(&round_id)))
    }

    /// Return the TTS output path for a specific request.
    pub fn tts_output_path(&self, request_id: Uuid) -> PathBuf {
        self.tts_dir
            .join(format!("{}.wav", uuid_to_path(&request_id)))
    }

    /// Build paths from an explicit tool directory (used by tests and the
    /// audio spike binary).
    pub fn from_tool_dir(tool_dir: PathBuf, data_dir: PathBuf) -> Result<Self, DatabasePathError> {
        let recordings_dir = data_dir.join("recordings");
        let tts_dir = data_dir.join("tts");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path(&data_dir)?;
        let tool_directory_source = ToolDirectorySource::DevFallback;

        Ok(Self {
            tool_dir,
            db_path,
            recordings_dir,
            tts_dir,
            temp_dir,
            is_portable: false,
            tool_directory_source,
        })
    }

    /// Resolve paths from injected inputs (testable without env vars).
    pub fn resolve_from_input(input: PathResolutionInput) -> Result<Self, DatabasePathError> {
        let options = ToolDirOptions {
            env_override: None,
            resource_dir: input.resource_dir.clone(),
            allow_dev_fallback: true,
        };
        let (tool_dir, tool_directory_source, distribution_mode) =
            resolve_tool_dir_with_options(&input.exe_dir, &options);
        let is_portable = distribution_mode == ToolDistributionMode::Portable;

        // Persistent data ALWAYS goes to app_data_dir — never exe_dir/data.
        let data_dir = input.app_data_dir.clone();

        let recordings_dir = data_dir.join("recordings");
        let tts_dir = data_dir.join("tts");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path(&data_dir)?;

        Ok(Self {
            tool_dir,
            db_path,
            recordings_dir,
            tts_dir,
            temp_dir,
            is_portable,
            tool_directory_source,
        })
    }

    /// Canonical constructor used at application startup.
    /// Uses `app_data_dir` (LOCALAPPDATA) for persistent storage — never
    /// `exe_dir/data`, even in portable mode.
    pub fn resolve() -> Result<Self, DatabasePathError> {
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let (tool_dir, tool_directory_source, distribution_mode) = resolve_tool_dir(&exe_dir);
        let is_portable = distribution_mode == ToolDistributionMode::Portable;

        // Persistent data always uses LOCALAPPDATA — never exe_dir/data.
        let data_dir = if let Ok(local) = env::var("LOCALAPPDATA") {
            PathBuf::from(local).join("ai-interviewer")
        } else {
            exe_dir.join("data")
        };
        let recordings_dir = data_dir.join("recordings");
        let tts_dir = data_dir.join("tts");
        let temp_dir = data_dir.join("temp");
        let db_path = resolve_database_path(&data_dir)?;

        Ok(Self {
            tool_dir,
            db_path,
            recordings_dir,
            tts_dir,
            temp_dir,
            is_portable,
            tool_directory_source,
        })
    }

    /// Produce the compact config sent to the frontend.
    pub fn to_app_config(&self) -> AppConfig {
        let readiness = self.validate_readiness();
        AppConfig {
            tool_dir: self.tool_dir.display().to_string(),
            db_path: self.db_path.display().to_string(),
            recordings_dir: self.recordings_dir.display().to_string(),
            tts_dir: self.tts_dir.display().to_string(),
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
        std::fs::create_dir_all(&self.tts_dir)
            .map_err(|e| format!("Failed to create tts dir: {e}"))?;
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

/// Resolve the tool directory from injected options.  This is the single
/// canonical resolver — all callers should use this function.
///
/// Resolution order:
/// 1. `AI_INTERVIEWER_TOOLS` env var (or `options.env_override`)
/// 2. Tauri bundled resources (`options.resource_dir` → `tools/`)
/// 3. Portable layout next to the exe (`exe_dir/tools/`)
/// 4. Dev fallback (debug builds only, when `options.allow_dev_fallback`)
pub fn resolve_tool_dir_with_options(
    exe_dir: &Path,
    options: &ToolDirOptions,
) -> (PathBuf, ToolDirectorySource, ToolDistributionMode) {
    // 1. Explicit env override
    if let Some(ref val) = options.env_override {
        let p = PathBuf::from(val);
        if p.exists() {
            return (
                p.clone(),
                ToolDirectorySource::EnvVar { value: val.clone() },
                ToolDistributionMode::EnvVar { value: val.clone() },
            );
        }
    }

    // 2. Tauri bundled resources — only when an explicit resource_dir is
    //    provided (from Tauri's app.path().resource_dir()).
    if let Some(ref res_dir) = options.resource_dir {
        let bundled = res_dir.join("tools");
        if bundled.exists() {
            return (
                bundled,
                ToolDirectorySource::Bundled,
                ToolDistributionMode::Bundled,
            );
        }
    }

    // 3. Portable layout next to the exe
    let portable = exe_dir.join("tools");
    if portable.exists() {
        return (
            portable,
            ToolDirectorySource::Portable {
                exe_dir: exe_dir.display().to_string(),
            },
            ToolDistributionMode::Portable,
        );
    }

    // 4. Dev fallback
    if options.allow_dev_fallback {
        #[cfg(debug_assertions)]
        {
            let dev = dev_tools_dir();
            if dev.exists() {
                return (
                    dev,
                    ToolDirectorySource::DevFallback,
                    ToolDistributionMode::DevFallback,
                );
            }
        }
    }

    (
        exe_dir.join("tools"),
        ToolDirectorySource::Unresolved,
        ToolDistributionMode::Unresolved,
    )
}

/// Resolve the tool directory using environment variables and exe path.
/// Prefer `resolve_tool_dir_with_options` for testable code.
pub fn resolve_tool_dir(exe_dir: &Path) -> (PathBuf, ToolDirectorySource, ToolDistributionMode) {
    let options = ToolDirOptions {
        env_override: None,
        resource_dir: None,
        allow_dev_fallback: true,
    };
    resolve_tool_dir_with_options(exe_dir, &options)
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

/// Fully-resolved paths to all external tools.
#[derive(Debug, Clone)]
pub struct ResolvedTools {
    pub piper_bin: Option<PathBuf>,
    pub piper_model: Option<PathBuf>,
    pub whisper_bin: Option<PathBuf>,
    pub whisper_model: Option<PathBuf>,
}

impl ResolvedTools {
    /// All critical tool paths are present.
    pub fn ready(&self) -> bool {
        self.piper_bin.is_some()
            && self.piper_model.is_some()
            && self.whisper_bin.is_some()
            && self.whisper_model.is_some()
    }
}

/// Resolve all tool paths in a single call.  Supports both canonical and
/// legacy layouts.  Returns a `ResolvedTools` where each field is `None`
/// if that component was not found.
pub fn resolve_tools(tool_dir: &Path) -> ResolvedTools {
    // Piper binary — canonical then legacy
    let piper_bin = {
        let canonical = tool_dir.join("piper").join("piper.exe");
        let legacy = tool_dir.join("piper").join("piper").join("piper.exe");
        if canonical.exists() {
            Some(canonical)
        } else if legacy.exists() {
            Some(legacy)
        } else {
            None
        }
    };

    // Piper model — canonical then legacy
    let piper_model = {
        let canonical = tool_dir.join("piper").join("model.onnx");
        let legacy = tool_dir.join("piper-models").join("en_US-amy-medium.onnx");
        if canonical.exists() {
            Some(canonical)
        } else if legacy.exists() {
            Some(legacy)
        } else {
            None
        }
    };

    // Whisper binary
    let whisper_bin = {
        let p = tool_dir.join("whisper").join("Release").join("main.exe");
        if p.exists() {
            Some(p)
        } else {
            None
        }
    };

    // Whisper model
    let whisper_model = {
        let p = tool_dir.join("models").join("ggml-tiny.en.bin");
        if p.exists() {
            Some(p)
        } else {
            None
        }
    };

    ResolvedTools {
        piper_bin,
        piper_model,
        whisper_bin,
        whisper_model,
    }
}

/// Backward-compatible wrapper that returns just the Piper paths.
/// Prefer `resolve_tools` for new code.
pub fn resolve_piper_paths(tool_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let tools = resolve_tools(tool_dir);
    (tools.piper_bin, tools.piper_model)
}

/// Backward-compatible wrapper that returns just the Whisper binary path.
/// Prefer `resolve_tools` for new code.
pub fn resolve_whisper_path(tool_dir: &Path) -> Option<PathBuf> {
    resolve_tools(tool_dir).whisper_bin
}

/// Backward-compatible wrapper that returns just the Whisper model path.
/// Prefer `resolve_tools` for new code.
pub fn resolve_whisper_model_path(tool_dir: &Path) -> Option<PathBuf> {
    resolve_tools(tool_dir).whisper_model
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
/// Uses Tauri's `resource_dir()` for bundled tool lookup in production.
pub fn resolve_app_paths(app: &tauri::AppHandle) -> Result<PathsState, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Tauri app_data_dir() failed (fatal): {e}"))?;

    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("Tauri resource_dir() failed: {e}"))?;

    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let input = PathResolutionInput {
        exe_dir,
        app_data_dir,
        resource_dir: Some(resource_dir),
    };
    let app_paths =
        AppPaths::resolve_from_input(input).map_err(|e| format!("Path resolution failed: {e}"))?;
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

        let paths = AppPaths::from_tool_dir(tool.clone(), data.clone()).unwrap();
        assert_eq!(paths.tool_dir, tool);
        assert_eq!(paths.db_path, data.join("interviews.db"));
        assert_eq!(paths.recordings_dir, data.join("recordings"));
        assert_eq!(paths.temp_dir, data.join("temp"));
        assert!(!paths.is_portable);
    }

    #[test]
    fn portable_detection_via_tool_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path();
        // Simulate portable layout: exe_dir/tools exists
        let tools_dir = exe_dir.join("tools");
        fs::create_dir_all(&tools_dir).unwrap();

        let opts = ToolDirOptions {
            env_override: None,
            resource_dir: None,
            allow_dev_fallback: false,
        };
        let (tool_dir, _, mode) = resolve_tool_dir_with_options(exe_dir, &opts);
        assert_eq!(mode, ToolDistributionMode::Portable);
        assert_eq!(tool_dir, tools_dir);

        // Portable mode should be detected in AppPaths
        let app_data = tmp.path().join("app_data");
        fs::create_dir_all(&app_data).unwrap();
        let input = PathResolutionInput {
            exe_dir: exe_dir.to_path_buf(),
            app_data_dir: app_data.clone(),
            resource_dir: None,
        };
        let paths = AppPaths::resolve_from_input(input).unwrap();
        assert!(paths.is_portable);
        // Data dir should ALWAYS be app_data_dir, never exe_dir/data
        assert_eq!(paths.recordings_dir, app_data.join("recordings"));
    }

    #[test]
    fn to_app_config_includes_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        fs::create_dir_all(&data).unwrap();

        let paths = AppPaths::from_tool_dir(tool, data).unwrap();
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

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
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

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
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

        let paths = AppPaths::from_tool_dir(tool, data).unwrap();
        paths.ensure_directories().unwrap();
        assert!(paths.recordings_dir.exists());
        assert!(paths.tts_dir.exists());
        assert!(paths.temp_dir.exists());
    }

    #[test]
    fn validate_path_component_rejects_dotdot() {
        assert!(validate_path_component("..").is_err());
        assert!(validate_path_component(".").is_err());
    }

    #[test]
    fn validate_path_component_rejects_slashes() {
        assert!(validate_path_component("a/b").is_err());
        assert!(validate_path_component("a\\b").is_err());
        assert!(validate_path_component("a\0b").is_err());
    }

    #[test]
    fn validate_path_component_accepts_normal() {
        assert!(validate_path_component("my-session").is_ok());
        assert!(validate_path_component("abc123").is_ok());
    }

    #[test]
    fn session_recordings_dir_returns_valid_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        let paths = AppPaths::from_tool_dir(tool, data).unwrap();

        let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let result = paths.session_recordings_dir(session_id);
        assert!(result.ends_with("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn session_temp_dir_returns_valid_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        let paths = AppPaths::from_tool_dir(tool, data).unwrap();

        let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let result = paths.session_temp_dir(session_id);
        assert!(result.ends_with("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn round_audio_path_returns_valid_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        let paths = AppPaths::from_tool_dir(tool, data).unwrap();

        let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let round_id = Uuid::parse_str("660e8400-e29b-41d4-a716-446655440001").unwrap();
        let result = paths.round_audio_path(session_id, round_id);
        let path_str = result.to_string_lossy().to_string();
        assert!(path_str.contains("550e8400"));
        assert!(path_str.contains("660e8400"));
        assert!(path_str.ends_with(".wav"));
    }

    #[test]
    fn tts_output_path_returns_valid_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        let data = tmp.path().join("data");
        fs::create_dir_all(&tool).unwrap();
        let paths = AppPaths::from_tool_dir(tool, data).unwrap();

        let request_id = Uuid::parse_str("770e8400-e29b-41d4-a716-446655440002").unwrap();
        let result = paths.tts_output_path(request_id);
        let path_str = result.to_string_lossy().to_string();
        assert!(path_str.contains("tts"));
        assert!(path_str.ends_with(".wav"));
    }

    #[test]
    fn resolve_tools_canonical_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Canonical piper layout
        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
        fs::write(tool.join("piper").join("model.onnx"), b"").unwrap();
        // Whisper
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        // Whisper model
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let tools = resolve_tools(&tool);
        assert!(tools.ready());
        assert_eq!(tools.piper_bin, Some(tool.join("piper").join("piper.exe")));
        assert_eq!(
            tools.piper_model,
            Some(tool.join("piper").join("model.onnx"))
        );
        assert_eq!(
            tools.whisper_bin,
            Some(tool.join("whisper").join("Release").join("main.exe"))
        );
        assert_eq!(
            tools.whisper_model,
            Some(tool.join("models").join("ggml-tiny.en.bin"))
        );
    }

    #[test]
    fn resolve_tools_legacy_piper_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Legacy piper layout
        fs::create_dir_all(tool.join("piper").join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper").join("piper.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("piper-models")).unwrap();
        fs::write(tool.join("piper-models").join("en_US-amy-medium.onnx"), b"").unwrap();
        // Whisper
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let tools = resolve_tools(&tool);
        assert!(tools.ready());
        assert_eq!(
            tools.piper_bin,
            Some(tool.join("piper").join("piper").join("piper.exe"))
        );
        assert_eq!(
            tools.piper_model,
            Some(tool.join("piper-models").join("en_US-amy-medium.onnx"))
        );
    }

    #[test]
    fn resolve_tools_missing_all() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        fs::create_dir_all(&tool).unwrap();

        let tools = resolve_tools(&tool);
        assert!(!tools.ready());
        assert!(tools.piper_bin.is_none());
        assert!(tools.piper_model.is_none());
        assert!(tools.whisper_bin.is_none());
        assert!(tools.whisper_model.is_none());
    }
}
