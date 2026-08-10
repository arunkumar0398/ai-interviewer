use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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
    /// Override for the `AI_INTERVIEWER_TOOLS` env var. Production reads
    /// `var_os("AI_INTERVIEWER_TOOLS")` into this field; tests supply
    /// synthetic paths directly — keeping the resolver pure.
    pub env_tools_dir: Option<PathBuf>,
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

/// One coherent Piper runtime layout. Readiness and runtime selection must
/// agree on which layout is in use — a mixed canonical/legacy tree must NOT
/// report ready (P1-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiperLayout {
    Canonical,
    Legacy,
}

/// Resolve which Piper layout is selected, mirroring the runtime resolver
/// (`resolve_tools` prefers the canonical executable). Returns `None` when no
/// Piper executable exists in either layout.
pub fn resolve_piper_layout(tool_dir: &Path) -> Option<PiperLayout> {
    let piper_dir = tool_dir.join("piper");
    if piper_dir.join("piper.exe").exists() {
        Some(PiperLayout::Canonical)
    } else if piper_dir.join("piper").join("piper.exe").exists() {
        Some(PiperLayout::Legacy)
    } else {
        None
    }
}

/// (relative asset path, issue code, message) triples for the canonical
/// (relative asset path, issue code, message) triples for the Piper runtime
/// companions that must live in the SELECTED layout's runtime directory
/// (canonical `tools/piper/`, legacy `tools/piper/piper/`). The model/config
/// pair is validated separately via `ResolvedPiper` so executable and model
/// always resolve from the SAME coherent layout (P1-3).
const PIPER_RUNTIME_ASSETS: &[(&str, &str, &str)] = &[
    (
        "piper.exe",
        "PIPER_BINARY_MISSING",
        "Piper TTS binary not found",
    ),
    (
        "espeak-ng.dll",
        "PIPER_ESPEAK_DLL_MISSING",
        "Piper espeak-ng DLL not found",
    ),
    (
        "piper_phonemize.dll",
        "PIPER_PHONEMIZE_DLL_MISSING",
        "Piper phonemize DLL not found",
    ),
    (
        "onnxruntime.dll",
        "PIPER_ONNX_RUNTIME_MISSING",
        "Piper ONNX runtime DLL not found",
    ),
    (
        "onnxruntime_providers_shared.dll",
        "PIPER_ONNX_PROVIDER_MISSING",
        "Piper ONNX provider DLL not found",
    ),
    (
        "espeak-ng-data/phontab",
        "PIPER_ESPEAK_DATA_MISSING",
        "Piper espeak-ng data not found",
    ),
];

/// One coherent, fully-resolved Piper runtime (P1-3). Executable, model, and
/// model config ALWAYS come from the same layout — there is no independent
/// canonical-first model fallback after the layout is selected. Readiness and
/// runtime execution both derive from this resolver, so they always agree.
#[derive(Debug, Clone)]
pub struct ResolvedPiper {
    pub layout: PiperLayout,
    pub executable: PathBuf,
    pub model: PathBuf,
    pub model_config: PathBuf,
    pub runtime_dir: PathBuf,
}

/// Resolve the coherent Piper runtime (executable + model + config + runtime
/// directory) for the selected layout. Returns `None` when no Piper executable
/// exists in either layout. The returned paths are the layout's canonical
/// expectations — callers check `.exists()` as needed.
pub fn resolve_piper(tool_dir: &Path) -> Option<ResolvedPiper> {
    match resolve_piper_layout(tool_dir)? {
        PiperLayout::Canonical => {
            let dir = tool_dir.join("piper");
            Some(ResolvedPiper {
                layout: PiperLayout::Canonical,
                executable: dir.join("piper.exe"),
                model: dir.join("model.onnx"),
                model_config: dir.join("model.onnx.json"),
                runtime_dir: dir,
            })
        }
        PiperLayout::Legacy => {
            let runtime_dir = tool_dir.join("piper").join("piper");
            let models_dir = tool_dir.join("piper-models");
            Some(ResolvedPiper {
                layout: PiperLayout::Legacy,
                executable: runtime_dir.join("piper.exe"),
                model: models_dir.join("en_US-amy-medium.onnx"),
                model_config: models_dir.join("en_US-amy-medium.onnx.json"),
                runtime_dir,
            })
        }
    }
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

    /// Quarantine directory for ambiguous/unreconciled candidate evidence
    /// (RC-4). Evidence that cannot be conclusively classified is moved here
    /// — preserved for review, never destroyed. Startup reconciliation
    /// reports (but never deletes) its contents.
    pub fn quarantine_dir(&self) -> PathBuf {
        self.recordings_dir
            .parent()
            .unwrap_or(&self.recordings_dir)
            .join("quarantine")
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
            env_override: input
                .env_tools_dir
                .as_ref()
                .map(|p| p.display().to_string()),
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

    /// Validate that critical binaries, models, AND Piper's required runtime
    /// companions are present (P2-3/P1-2). Packaging requires the full Piper
    /// runtime (espeak-ng.dll, piper_phonemize.dll, onnxruntime.dll,
    /// onnxruntime_providers_shared.dll, espeak-ng-data/) — a damaged or
    /// hand-supplied AI_INTERVIEWER_TOOLS directory that only has the exe
    /// would otherwise report `ready = true` and then fail at runtime.
    ///
    /// Readiness resolves ONE coherent Piper layout first (canonical
    /// preferred, matching the runtime resolver) via `resolve_piper` and
    /// validates every required asset against that selected layout only — the
    /// SAME resolver the runtime uses to pick the executable and model. A
    /// mixed tree — canonical executable with only legacy DLLs, a canonical
    /// model paired with a legacy config, or a stray canonical model next to
    /// a selected legacy runtime — must NOT report ready. Returns the
    /// aggregate readiness state — callers decide whether to surface errors
    /// or degrade gracefully.
    pub fn validate_readiness(&self) -> AppReadiness {
        let mut issues = Vec::new();

        // ONE coherent resolver for runtime AND readiness (P1-3): the
        // selected layout determines the executable, the model/config pair,
        // and the runtime directory. No independent canonical-first model
        // fallback after layout selection.
        match resolve_piper(&self.tool_dir) {
            Some(piper) => {
                // All runtime companions come from the selected layout's
                // runtime directory.
                for (asset, code, message) in PIPER_RUNTIME_ASSETS {
                    let path = piper.runtime_dir.join(asset);
                    if !path.is_file() {
                        issues.push(AppConfigurationIssue {
                            code: (*code).into(),
                            message: (*message).into(),
                            expected_path: Some(path.display().to_string()),
                        });
                    }
                }
                // The selected layout's model/config pair.
                if !piper.model.is_file() {
                    issues.push(AppConfigurationIssue {
                        code: "PIPER_MODEL_MISSING".into(),
                        message: "Piper model not found".into(),
                        expected_path: Some(piper.model.display().to_string()),
                    });
                }
                if !piper.model_config.is_file() {
                    issues.push(AppConfigurationIssue {
                        code: "PIPER_MODEL_CONFIG_MISSING".into(),
                        message: "Piper model config not found".into(),
                        expected_path: Some(piper.model_config.display().to_string()),
                    });
                }
            }
            None => {
                // No Piper executable in either layout. Report the canonical
                // expectations (binary, model, config) as missing.
                let piper_dir = self.tool_dir.join("piper");
                for (asset, code, message) in [
                    (
                        "piper.exe",
                        "PIPER_BINARY_MISSING",
                        "Piper TTS binary not found",
                    ),
                    ("model.onnx", "PIPER_MODEL_MISSING", "Piper model not found"),
                    (
                        "model.onnx.json",
                        "PIPER_MODEL_CONFIG_MISSING",
                        "Piper model config not found",
                    ),
                ] {
                    let path = piper_dir.join(asset);
                    issues.push(AppConfigurationIssue {
                        code: code.into(),
                        message: message.into(),
                        expected_path: Some(path.display().to_string()),
                    });
                }
            }
        }

        // Whisper binary: tools/whisper/Release/main.exe
        let whisper_path = self
            .tool_dir
            .join("whisper")
            .join("Release")
            .join("main.exe");
        if !whisper_path.is_file() {
            issues.push(AppConfigurationIssue {
                code: "WHISPER_BINARY_MISSING".into(),
                message: "Whisper binary not found".into(),
                expected_path: Some(whisper_path.display().to_string()),
            });
        }

        // Whisper model: tools/models/ggml-tiny.en.bin
        let whisper_model = self.tool_dir.join("models").join("ggml-tiny.en.bin");
        if !whisper_model.is_file() {
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
    // Piper binary AND model resolve from ONE coherent layout (P1-3): the
    // selected layout decides both — a stray canonical model next to a
    // selected legacy runtime is never picked up. Each field is `Some` ONLY
    // when that asset actually exists (RC-5): `ready()` must never report
    // true while a critical asset (e.g. the Piper model) is missing.
    let (piper_bin, piper_model) = match resolve_piper(tool_dir) {
        Some(piper) => {
            let bin = piper.executable.exists().then_some(piper.executable);
            let model = piper.model.exists().then_some(piper.model);
            (bin, model)
        }
        None => (None, None),
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

/// Shared state managed by Tauri, holds the resolved `AppPaths` plus a
/// CACHED runtime-integrity result (RC-6): critical pinned assets are hashed
/// once (first `get_app_config`), never on every readiness request.
pub struct PathsState {
    pub paths: AppPaths,
    pub(crate) integrity: OnceLock<RuntimeIntegrity>,
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
        env_tools_dir: env::var_os("AI_INTERVIEWER_TOOLS").map(PathBuf::from),
    };
    let app_paths =
        AppPaths::resolve_from_input(input).map_err(|e| format!("Path resolution failed: {e}"))?;
    app_paths.ensure_directories()?;
    Ok(PathsState {
        paths: app_paths,
        integrity: OnceLock::new(),
    })
}

/// Tauri command: return compact config (with readiness) to the frontend.
/// The runtime-integrity check is computed ONCE and cached (RC-6) — hashing
/// the multi-hundred-MB model assets on every readiness request is never
/// acceptable. Tool files are static for the lifetime of an app session, so
/// a single authoritative verification at first use is correct.
#[tauri::command]
pub fn get_app_config(paths: tauri::State<'_, PathsState>) -> AppConfig {
    let integrity = paths
        .integrity
        .get_or_init(|| verify_runtime_integrity(&paths.paths.tool_dir));
    let mut config = paths.paths.to_app_config();
    if !integrity.verified {
        config.readiness.ready = false;
        config
            .readiness
            .issues
            .extend(integrity.issues.iter().cloned());
    }
    config
}

// ---------------------------------------------------------------------------
// Runtime asset integrity (RC-6)
// ---------------------------------------------------------------------------

/// The authoritative tool manifest, embedded at compile time. This is the
/// SAME manifest packaging/CI verifies (`resources/tool-manifest.json`), so
/// runtime integrity matches the release policy exactly.
const TOOL_MANIFEST_JSON: &str = include_str!("../../resources/tool-manifest.json");

#[derive(Deserialize)]
struct ToolManifest {
    tools: HashMap<String, ManifestToolEntry>,
}

#[derive(Deserialize)]
struct ManifestToolEntry {
    #[serde(rename = "type")]
    tool_type: String,
    sha256: Option<String>,
    destination: Option<String>,
}

/// Result of the cached runtime-integrity verification (RC-6).
#[derive(Debug, Clone)]
pub(crate) struct RuntimeIntegrity {
    verified: bool,
    issues: Vec<AppConfigurationIssue>,
}

/// SHA-256 hex digest of a file.
pub fn sha256_hex(path: &Path) -> anyhow::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify a single file against its expected normalized SHA-256 hex.
fn verify_file_hash(path: &Path, expected_hex: &str) -> Result<(), String> {
    let actual =
        sha256_hex(path).map_err(|e| format!("failed to hash {}: {}", path.display(), e))?;
    if actual.eq_ignore_ascii_case(expected_hex.trim()) {
        Ok(())
    } else {
        Err(format!(
            "checksum mismatch for {} (expected {}, got {})",
            path.display(),
            expected_hex,
            actual
        ))
    }
}

/// RC-6: verify critical pinned assets against the authoritative manifest
/// hashes. Only `type: "file"` entries carry per-file hashes in the
/// manifest; `type: "archive"` entries hash the archive, not the extracted
/// contents, so extracted binaries/DLLs remain existence-validated (by
/// `validate_readiness`) — the manifest does not contain executable hashes
/// and none are invented here. Missing files are skipped (the existing
/// missing-asset readiness path reports them); a file that EXISTS but hashes
/// differently makes integrity fail — a modified/corrupted critical asset
/// can never report fully ready.
pub(crate) fn verify_runtime_integrity(tool_dir: &Path) -> RuntimeIntegrity {
    let mut issues = Vec::new();
    let manifest: ToolManifest = match serde_json::from_str(TOOL_MANIFEST_JSON) {
        Ok(m) => m,
        Err(e) => {
            // The embedded manifest is part of the binary — a parse failure
            // is a build integrity problem, reported rather than ignored.
            issues.push(AppConfigurationIssue {
                code: "MANIFEST_UNREADABLE".into(),
                message: format!("Tool manifest could not be parsed: {e}"),
                expected_path: None,
            });
            return RuntimeIntegrity {
                verified: false,
                issues,
            };
        }
    };

    for (name, entry) in &manifest.tools {
        if entry.tool_type != "file" {
            continue;
        }
        let Some(expected) = entry.sha256.as_deref() else {
            continue;
        };
        let Some(destination) = entry.destination.as_deref() else {
            continue;
        };
        let path = tool_dir.join(destination);
        if !path.is_file() {
            // Missing assets are reported by validate_readiness with the
            // existing missing-code behavior — do not duplicate.
            continue;
        }
        if let Err(msg) = verify_file_hash(&path, expected) {
            issues.push(AppConfigurationIssue {
                code: format!("{}_INTEGRITY", name.to_uppercase().replace('-', "_")),
                message: msg,
                expected_path: Some(path.display().to_string()),
            });
        }
    }

    RuntimeIntegrity {
        verified: issues.is_empty(),
        issues,
    }
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
            env_tools_dir: None,
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

    /// Build the complete canonical runtime tree used by the readiness tests.
    fn create_canonical_runtime(tool: &std::path::Path) {
        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::create_dir_all(tool.join("piper").join("espeak-ng-data")).unwrap();
        for name in [
            "piper.exe",
            "model.onnx",
            "model.onnx.json",
            "espeak-ng.dll",
            "piper_phonemize.dll",
            "onnxruntime.dll",
            "onnxruntime_providers_shared.dll",
        ] {
            fs::write(tool.join("piper").join(name), b"").unwrap();
        }
        fs::write(
            tool.join("piper").join("espeak-ng-data").join("phontab"),
            b"",
        )
        .unwrap();
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();
    }

    /// All Piper runtime companions present → ready (P2-3).
    #[test]
    fn validate_readiness_passes_when_all_present() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        create_canonical_runtime(&tool);

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(readiness.ready);
        assert!(readiness.issues.is_empty());
    }

    /// Each mandatory Piper asset, removed individually, must flip readiness
    /// to false with its specific issue code (P2-3).
    #[test]
    fn validate_readiness_fails_when_each_piper_companion_missing() {
        for (asset, code) in [
            ("piper.exe", "PIPER_BINARY_MISSING"),
            ("model.onnx", "PIPER_MODEL_MISSING"),
            ("model.onnx.json", "PIPER_MODEL_CONFIG_MISSING"),
            ("espeak-ng.dll", "PIPER_ESPEAK_DLL_MISSING"),
            ("piper_phonemize.dll", "PIPER_PHONEMIZE_DLL_MISSING"),
            ("onnxruntime.dll", "PIPER_ONNX_RUNTIME_MISSING"),
            (
                "onnxruntime_providers_shared.dll",
                "PIPER_ONNX_PROVIDER_MISSING",
            ),
            ("espeak-ng-data/phontab", "PIPER_ESPEAK_DATA_MISSING"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let tool = tmp.path().join("tools");
            create_canonical_runtime(&tool);
            // Remove exactly this one asset.
            fs::remove_file(tool.join("piper").join(asset)).unwrap();

            let paths = AppPaths::from_tool_dir(tool.clone(), tmp.path().join("data")).unwrap();
            let readiness = paths.validate_readiness();
            assert!(!readiness.ready, "removing {} must flip readiness", asset);
            let issue = readiness
                .issues
                .iter()
                .find(|i| i.code == code)
                .unwrap_or_else(|| panic!("missing issue code {} for {}", code, asset)); // Compare the final path component (asset may contain a subdir
                                                                                         // like "espeak-ng-data/phontab", and separators differ per OS).
            let file_name = asset.rsplit('/').next().unwrap_or(asset);
            assert!(
                issue
                    .expected_path
                    .as_deref()
                    .unwrap_or("")
                    .ends_with(file_name),
                "expected_path must name the missing asset, got {:?}",
                issue.expected_path
            );
        }
    }

    /// A damaged tools dir with only the exe and model (no companions) must
    /// NOT report ready (P2-3): Piper would fail at runtime.
    #[test]
    fn validate_readiness_rejects_exe_only_piper_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
        fs::write(tool.join("piper").join("model.onnx"), b"").unwrap();
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(!readiness.ready, "exe-only piper dir must not be ready");
        for code in [
            "PIPER_MODEL_CONFIG_MISSING",
            "PIPER_ESPEAK_DLL_MISSING",
            "PIPER_PHONEMIZE_DLL_MISSING",
            "PIPER_ONNX_RUNTIME_MISSING",
            "PIPER_ONNX_PROVIDER_MISSING",
            "PIPER_ESPEAK_DATA_MISSING",
        ] {
            assert!(
                readiness.issues.iter().any(|i| i.code == code),
                "missing companion issue {} must be reported",
                code
            );
        }
    }

    /// Build the complete legacy runtime tree used by the readiness tests.
    fn create_legacy_runtime(tool: &std::path::Path) {
        fs::create_dir_all(tool.join("piper").join("piper").join("espeak-ng-data")).unwrap();
        for name in [
            "piper.exe",
            "espeak-ng.dll",
            "piper_phonemize.dll",
            "onnxruntime.dll",
            "onnxruntime_providers_shared.dll",
        ] {
            fs::write(tool.join("piper").join("piper").join(name), b"").unwrap();
        }
        fs::write(
            tool.join("piper")
                .join("piper")
                .join("espeak-ng-data")
                .join("phontab"),
            b"",
        )
        .unwrap();
        fs::create_dir_all(tool.join("piper-models")).unwrap();
        fs::write(tool.join("piper-models").join("en_US-amy-medium.onnx"), b"").unwrap();
        fs::write(
            tool.join("piper-models").join("en_US-amy-medium.onnx.json"),
            b"",
        )
        .unwrap();
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();
    }

    /// P1-2: a mixed tree — canonical executable with only legacy DLLs — must
    /// NOT report ready. Readiness resolves the canonical layout (selected by
    /// the runtime resolver) and refuses to borrow companions from legacy.
    #[test]
    fn validate_readiness_rejects_canonical_exe_with_only_legacy_dlls() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Canonical exe/model/config present, but NO canonical DLLs.
        fs::create_dir_all(tool.join("piper").join("espeak-ng-data")).unwrap();
        fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
        fs::write(tool.join("piper").join("model.onnx"), b"").unwrap();
        fs::write(tool.join("piper").join("model.onnx.json"), b"").unwrap();
        fs::write(
            tool.join("piper").join("espeak-ng-data").join("phontab"),
            b"",
        )
        .unwrap();
        // Only the LEGACY runtime directory has the DLLs.
        fs::create_dir_all(tool.join("piper").join("piper")).unwrap();
        for name in [
            "espeak-ng.dll",
            "piper_phonemize.dll",
            "onnxruntime.dll",
            "onnxruntime_providers_shared.dll",
        ] {
            fs::write(tool.join("piper").join("piper").join(name), b"").unwrap();
        }
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        assert_eq!(
            resolve_piper_layout(&tool),
            Some(PiperLayout::Canonical),
            "canonical exe selects the canonical layout"
        );
        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(
            !readiness.ready,
            "mixed canonical exe + legacy DLLs must not be ready"
        );
        for code in [
            "PIPER_ESPEAK_DLL_MISSING",
            "PIPER_PHONEMIZE_DLL_MISSING",
            "PIPER_ONNX_RUNTIME_MISSING",
            "PIPER_ONNX_PROVIDER_MISSING",
        ] {
            assert!(
                readiness.issues.iter().any(|i| i.code == code),
                "missing canonical companion issue {} must be reported",
                code
            );
        }
    }

    /// P1-2: canonical model paired with only a legacy config is incoherent
    /// and must NOT report ready.
    #[test]
    fn validate_readiness_rejects_canonical_model_with_only_legacy_config() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Canonical runtime WITHOUT the canonical config.
        fs::create_dir_all(tool.join("piper").join("espeak-ng-data")).unwrap();
        for name in [
            "piper.exe",
            "model.onnx",
            "espeak-ng.dll",
            "piper_phonemize.dll",
            "onnxruntime.dll",
            "onnxruntime_providers_shared.dll",
        ] {
            fs::write(tool.join("piper").join(name), b"").unwrap();
        }
        fs::write(
            tool.join("piper").join("espeak-ng-data").join("phontab"),
            b"",
        )
        .unwrap();
        // Only the LEGACY model config exists.
        fs::create_dir_all(tool.join("piper-models")).unwrap();
        fs::write(
            tool.join("piper-models").join("en_US-amy-medium.onnx.json"),
            b"",
        )
        .unwrap();
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(
            !readiness.ready,
            "canonical model + legacy-only config must not be ready"
        );
        assert!(
            readiness
                .issues
                .iter()
                .any(|i| i.code == "PIPER_MODEL_CONFIG_MISSING"),
            "missing canonical model config must be reported"
        );
    }

    /// P1-2/P1-3: when both layouts are valid, canonical remains selected, and
    /// the layout resolver agrees with the coherent runtime resolver
    /// (executable + model + config all from the same layout).
    #[test]
    fn resolve_piper_layout_agrees_with_runtime_resolver() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Canonical only.
        create_canonical_runtime(&tool);
        assert_eq!(resolve_piper_layout(&tool), Some(PiperLayout::Canonical));
        let piper = resolve_piper(&tool).unwrap();
        assert_eq!(piper.layout, PiperLayout::Canonical);
        assert_eq!(piper.executable, tool.join("piper").join("piper.exe"));
        assert_eq!(piper.model, tool.join("piper").join("model.onnx"));
        assert_eq!(
            piper.model_config,
            tool.join("piper").join("model.onnx.json")
        );
        let resolved = resolve_tools(&tool);
        assert_eq!(resolved.piper_bin, Some(piper.executable));
        assert_eq!(resolved.piper_model, Some(piper.model));

        // Both layouts valid -> canonical still selected.
        create_legacy_runtime(&tool);
        assert_eq!(
            resolve_piper_layout(&tool),
            Some(PiperLayout::Canonical),
            "canonical must remain selected when both layouts are valid"
        );
        let piper = resolve_piper(&tool).unwrap();
        assert_eq!(piper.layout, PiperLayout::Canonical);
        let resolved = resolve_tools(&tool);
        assert_eq!(
            resolved.piper_bin,
            Some(tool.join("piper").join("piper.exe")),
            "runtime resolver must agree: canonical exe selected"
        );
        assert_eq!(
            resolved.piper_model,
            Some(tool.join("piper").join("model.onnx"))
        );

        // Legacy only (no canonical exe) -> legacy selected.
        let tmp2 = tempfile::tempdir().unwrap();
        let tool2 = tmp2.path().join("tools");
        create_legacy_runtime(&tool2);
        assert_eq!(resolve_piper_layout(&tool2), Some(PiperLayout::Legacy));
        let piper = resolve_piper(&tool2).unwrap();
        assert_eq!(piper.layout, PiperLayout::Legacy);
        assert_eq!(
            piper.executable,
            tool2.join("piper").join("piper").join("piper.exe")
        );
        assert_eq!(
            piper.model,
            tool2.join("piper-models").join("en_US-amy-medium.onnx")
        );
        assert_eq!(
            piper.model_config,
            tool2
                .join("piper-models")
                .join("en_US-amy-medium.onnx.json")
        );
        let resolved = resolve_tools(&tool2);
        assert_eq!(resolved.piper_bin, Some(piper.executable));
        assert_eq!(resolved.piper_model, Some(piper.model));

        // No exe anywhere -> no layout.
        let tmp3 = tempfile::tempdir().unwrap();
        let tool3 = tmp3.path().join("tools");
        fs::create_dir_all(&tool3).unwrap();
        assert_eq!(resolve_piper_layout(&tool3), None);
        assert!(resolve_piper(&tool3).is_none());
    }

    /// P1-3: a stray canonical model next to a selected legacy runtime must
    /// NOT be picked up — the runtime keeps using the legacy model/config pair
    /// and readiness validates that same pair.
    #[test]
    fn resolve_piper_ignores_stray_canonical_model_for_legacy_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // Complete legacy runtime/model/config.
        create_legacy_runtime(&tool);
        // Stray canonical model (no canonical executable, no canonical
        // config) — must be ignored.
        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::write(tool.join("piper").join("model.onnx"), b"stray").unwrap();

        assert_eq!(resolve_piper_layout(&tool), Some(PiperLayout::Legacy));
        let piper = resolve_piper(&tool).unwrap();
        assert_eq!(piper.layout, PiperLayout::Legacy);
        assert_eq!(
            piper.model,
            tool.join("piper-models").join("en_US-amy-medium.onnx"),
            "runtime must select the legacy model, not the stray canonical one"
        );
        let resolved = resolve_tools(&tool);
        assert_eq!(resolved.piper_model, Some(piper.model));

        // Readiness validates the SAME legacy pair and passes.
        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(readiness.ready, "coherent legacy runtime must be ready");
    }

    /// The legacy layout, when fully populated, is still valid (P2-3).
    #[test]
    fn validate_readiness_legacy_layout_fully_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        create_legacy_runtime(&tool);

        let paths = AppPaths::from_tool_dir(tool, tmp.path().join("data")).unwrap();
        let readiness = paths.validate_readiness();
        assert!(readiness.ready, "fully-populated legacy layout must pass");
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

    /// RC-5: `ResolvedTools::ready()` must reflect actual asset EXISTENCE,
    /// never merely the layout's expected paths. Each field is `Some` only
    /// when the file exists; any missing critical asset flips ready() to
    /// false. Coherent legacy layout still reports ready when complete.
    #[test]
    fn resolve_tools_ready_reflects_asset_existence() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");

        // 1. Piper executable present + Piper model MISSING -> not ready.
        fs::create_dir_all(tool.join("piper")).unwrap();
        fs::write(tool.join("piper").join("piper.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("whisper").join("Release")).unwrap();
        fs::write(tool.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool.join("models")).unwrap();
        fs::write(tool.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let tools = resolve_tools(&tool);
        assert_eq!(tools.piper_bin, Some(tool.join("piper").join("piper.exe")));
        assert!(
            tools.piper_model.is_none(),
            "missing Piper model must resolve to None"
        );
        assert!(!tools.ready(), "piper exe without model must not be ready");

        // 2. Piper model present + Piper executable MISSING -> not ready
        // (no coherent layout is selected, so neither Piper field is set).
        let tmp2 = tempfile::tempdir().unwrap();
        let tool2 = tmp2.path().join("tools");
        fs::create_dir_all(tool2.join("piper")).unwrap();
        fs::write(tool2.join("piper").join("model.onnx"), b"").unwrap();
        fs::create_dir_all(tool2.join("whisper").join("Release")).unwrap();
        fs::write(tool2.join("whisper").join("Release").join("main.exe"), b"").unwrap();
        fs::create_dir_all(tool2.join("models")).unwrap();
        fs::write(tool2.join("models").join("ggml-tiny.en.bin"), b"").unwrap();

        let tools = resolve_tools(&tool2);
        assert!(
            tools.piper_bin.is_none(),
            "missing exe must resolve to None"
        );
        assert!(
            tools.piper_model.is_none(),
            "no coherent layout selected -> no model field"
        );
        assert!(!tools.ready(), "model without executable must not be ready");

        // 3. Whisper executable MISSING -> not ready.
        let tmp3 = tempfile::tempdir().unwrap();
        let tool3 = tmp3.path().join("tools");
        create_canonical_runtime(&tool3);
        fs::remove_file(tool3.join("whisper").join("Release").join("main.exe")).unwrap();
        let tools = resolve_tools(&tool3);
        assert!(tools.whisper_bin.is_none());
        assert!(!tools.ready(), "missing whisper exe must not be ready");

        // 4. Whisper model MISSING -> not ready.
        let tmp4 = tempfile::tempdir().unwrap();
        let tool4 = tmp4.path().join("tools");
        create_canonical_runtime(&tool4);
        fs::remove_file(tool4.join("models").join("ggml-tiny.en.bin")).unwrap();
        let tools = resolve_tools(&tool4);
        assert!(tools.whisper_model.is_none());
        assert!(!tools.ready(), "missing whisper model must not be ready");

        // 5. Complete canonical tree -> ready.
        let tmp5 = tempfile::tempdir().unwrap();
        let tool5 = tmp5.path().join("tools");
        create_canonical_runtime(&tool5);
        assert!(
            resolve_tools(&tool5).ready(),
            "complete canonical must be ready"
        );

        // 6. Complete coherent legacy tree -> ready.
        let tmp6 = tempfile::tempdir().unwrap();
        let tool6 = tmp6.path().join("tools");
        create_legacy_runtime(&tool6);
        assert!(
            resolve_tools(&tool6).ready(),
            "complete legacy must be ready"
        );
    }

    /// RC-6: a file whose hash matches the expected value passes.
    #[test]
    fn verify_file_hash_known_good_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        let content = b"known-good-content";
        fs::write(&path, content).unwrap();

        let expected = format!("{:x}", sha2::Sha256::digest(content));
        assert!(verify_file_hash(&path, &expected).is_ok());
    }

    /// RC-6: a one-byte modification flips the hash and fails verification.
    #[test]
    fn verify_file_hash_one_byte_change_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        let mut content = b"known-good-content".to_vec();
        fs::write(&path, &content).unwrap();
        let expected = format!("{:x}", sha2::Sha256::digest(&content));

        // Flip one byte.
        content[0] ^= 0xFF;
        fs::write(&path, &content).unwrap();
        let result = verify_file_hash(&path, &expected);
        assert!(result.is_err(), "one-byte change must fail verification");
        assert!(result.unwrap_err().contains("checksum mismatch"));
    }

    /// RC-6: a corrupted critical asset (model bytes changed) fails runtime
    /// integrity — a modified model can never report fully ready.
    #[test]
    fn verify_runtime_integrity_detects_corrupt_model() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        // Empty files cannot match the manifest hashes of the real models.
        create_canonical_runtime(&tool);

        let integrity = verify_runtime_integrity(&tool);
        assert!(
            !integrity.verified,
            "corrupt assets must fail runtime integrity"
        );
        let codes: Vec<&str> = integrity.issues.iter().map(|i| i.code.as_str()).collect();
        assert!(
            codes.iter().any(|c| c.contains("PIPER_MODEL_INTEGRITY")),
            "corrupt piper model must be reported, got {codes:?}"
        );
        assert!(
            codes.iter().any(|c| c.contains("WHISPER_MODEL_INTEGRITY")),
            "corrupt whisper model must be reported, got {codes:?}"
        );
    }

    /// RC-6: missing assets do not produce integrity issues (the existing
    /// missing-asset readiness path reports them) — integrity only rejects
    /// assets that EXIST but hash differently.
    #[test]
    fn verify_runtime_integrity_missing_assets_are_not_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = tmp.path().join("tools");
        fs::create_dir_all(&tool).unwrap();

        let integrity = verify_runtime_integrity(&tool);
        assert!(
            integrity.verified,
            "an empty tools dir has no corrupt assets — existence handles it"
        );
        assert!(integrity.issues.is_empty());
    }
}
