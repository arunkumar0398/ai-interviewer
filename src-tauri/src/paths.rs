use std::path::{Path, PathBuf};
use tauri::Manager;

/// Development fallback — external tools directory.
/// In production, tools live next to the executable under `tools/`.
const DEV_TOOLS_DIR: &str = r"D:\_Career\__ntingAcc-\_work\ai-interviewer-tools";

/// All resolved filesystem paths for the application.
#[derive(Debug, Clone)]
pub struct AppPaths {
    /// Root directory containing piper, whisper, models, etc.
    pub tool_dir: PathBuf,

    // Piper
    pub piper_bin: PathBuf,
    pub piper_model: PathBuf,

    // Whisper
    pub whisper_bin: PathBuf,
    pub whisper_model: PathBuf,

    // Persistent storage (per-user, in app_data_dir)
    pub db_path: PathBuf,
    pub recordings_dir: PathBuf,

    // Scratch space (cleared on startup)
    pub temp_dir: PathBuf,

    /// True when running from an extracted portable layout.
    pub is_portable: bool,
}

/// Serializable config returned to the frontend by `get_app_config`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppConfig {
    pub tool_dir: String,
    pub piper_bin: String,
    pub piper_model: String,
    pub whisper_bin: String,
    pub whisper_model: String,
    pub db_path: String,
    pub recordings_dir: String,
    pub temp_dir: String,
    pub is_portable: bool,
}

/// Wraps `AppPaths` for Tauri managed state.
pub struct PathsState {
    pub paths: AppPaths,
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve all application paths.
///
/// Strategy:
/// 1. **Portable** — `<exe_dir>/tools/` exists → use it as `tool_dir`.
/// 2. **Development** — fall back to `DEV_TOOLS_DIR`.
/// 3. **Canonical layout** — `piper/piper/piper.exe` + `piper/en_US-amy-medium.onnx`
///    (model co-located with binary).
/// 4. **Legacy fallback** — `piper/piper/piper.exe` + `piper-models/en_US-amy-medium.onnx`
///    with a diagnostic warning.
pub fn resolve_app_paths(app_handle: &tauri::AppHandle) -> anyhow::Result<AppPaths> {
    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine executable directory"))?
        .to_path_buf();

    // Persistent directories (always in app_data_dir)
    let app_data = app_handle
        .path()
        .app_data_dir()
        .map_err(|e| anyhow::anyhow!("Cannot resolve app_data_dir: {}", e))?;
    std::fs::create_dir_all(&app_data)?;

    let recordings_dir = app_data.join("recordings");
    std::fs::create_dir_all(&recordings_dir)?;

    let temp_dir = app_data.join("temp");
    std::fs::create_dir_all(&temp_dir)?;

    let db_path = app_data.join("interviewer.db");

    // Portable vs development tool_dir
    let portable_tools = exe_dir.join("tools");
    let (tool_dir, is_portable) = if portable_tools.exists() {
        (portable_tools, true)
    } else {
        let dev_tools = PathBuf::from(DEV_TOOLS_DIR);
        if dev_tools.exists() {
            eprintln!(
                "[paths] Portable tools/ not found next to exe, using development fallback: {}",
                DEV_TOOLS_DIR
            );
            (dev_tools, false)
        } else {
            // Neither exists — use portable path as default so callers get
            // a clear "file not found" error with the expected location.
            eprintln!(
                "[paths] WARNING: No tools directory found. Checked:\n  {}\n  {}",
                portable_tools.display(),
                DEV_TOOLS_DIR
            );
            (portable_tools, false)
        }
    };

    // Piper paths — canonical then legacy
    let (piper_bin, piper_model) = resolve_piper_paths(&tool_dir)?;
    let (whisper_bin, whisper_model) = resolve_whisper_paths(&tool_dir)?;

    Ok(AppPaths {
        tool_dir,
        piper_bin,
        piper_model,
        whisper_bin,
        whisper_model,
        db_path,
        recordings_dir,
        temp_dir,
        is_portable,
    })
}

/// Resolve Piper binary + model.
///
/// Canonical: `tool_dir/piper/piper/piper.exe` + `tool_dir/piper/en_US-amy-medium.onnx`
/// Legacy:   `tool_dir/piper/piper/piper.exe` + `tool_dir/piper-models/en_US-amy-medium.onnx`
fn resolve_piper_paths(tool_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let piper_bin = tool_dir.join("piper").join("piper").join("piper.exe");
    if !piper_bin.exists() {
        anyhow::bail!(
            "Piper binary not found at: {}\nSearched in tool_dir: {}",
            piper_bin.display(),
            tool_dir.display()
        );
    }

    // Canonical: model co-located with piper directory
    let canonical_model = tool_dir.join("piper").join("en_US-amy-medium.onnx");
    // Legacy: separate piper-models directory
    let legacy_model = tool_dir.join("piper-models").join("en_US-amy-medium.onnx");

    let piper_model = if canonical_model.exists() {
        canonical_model
    } else if legacy_model.exists() {
        eprintln!(
            "[paths] Piper model found in legacy location: {}\n  Expected: {}",
            legacy_model.display(),
            canonical_model.display()
        );
        legacy_model
    } else {
        anyhow::bail!(
            "Piper model not found.\nSearched:\n  {}\n  {}",
            canonical_model.display(),
            legacy_model.display()
        );
    };

    Ok((piper_bin, piper_model))
}

/// Resolve Whisper binary + model.
///
/// Layout: `tool_dir/whisper/Release/main.exe` + `tool_dir/models/ggml-tiny.en.bin`
fn resolve_whisper_paths(tool_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let whisper_bin = tool_dir.join("whisper").join("Release").join("main.exe");
    if !whisper_bin.exists() {
        anyhow::bail!("Whisper binary not found at: {}", whisper_bin.display());
    }

    let whisper_model = tool_dir.join("models").join("ggml-tiny.en.bin");
    if !whisper_model.exists() {
        anyhow::bail!("Whisper model not found at: {}", whisper_model.display());
    }

    Ok((whisper_bin, whisper_model))
}

// ---------------------------------------------------------------------------
// Tauri command
// ---------------------------------------------------------------------------

/// Return the resolved app config to the frontend.
/// This is the single readiness check — if it succeeds the app is bootstrapped.
#[tauri::command]
pub fn get_app_config(state: tauri::State<'_, PathsState>) -> AppConfig {
    let p = &state.paths;
    AppConfig {
        tool_dir: p.tool_dir.to_string_lossy().to_string(),
        piper_bin: p.piper_bin.to_string_lossy().to_string(),
        piper_model: p.piper_model.to_string_lossy().to_string(),
        whisper_bin: p.whisper_bin.to_string_lossy().to_string(),
        whisper_model: p.whisper_model.to_string_lossy().to_string(),
        db_path: p.db_path.to_string_lossy().to_string(),
        recordings_dir: p.recordings_dir.to_string_lossy().to_string(),
        temp_dir: p.temp_dir.to_string_lossy().to_string(),
        is_portable: p.is_portable,
    }
}
