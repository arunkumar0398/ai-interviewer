# Tools Layout

This document describes the expected directory layout for the AI Interviewer's
tool binaries. These tools are downloaded by `scripts/fetch-tools.ps1` and
verified by `scripts/verify-tools.ps1`.

## Directory Structure

```
tools/
  piper/
    piper.exe              # Piper TTS binary
    model.onnx             # Piper voice model
    model.onnx.json        # Piper model config
  piper-models/            # Legacy layout (deprecated)
    en_US-amy-medium.onnx
  whisper/
    Release/
      main.exe             # Whisper binary
  models/
    ggml-tiny.en.bin       # Whisper model
```

## Canonical vs Legacy Layouts

The application supports two layouts for Piper:

| Component | Canonical | Legacy (deprecated) |
|-----------|-----------|---------------------|
| Binary    | `tools/piper/piper.exe` | `tools/piper/piper/piper.exe` |
| Model     | `tools/piper/model.onnx` | `tools/piper-models/en_US-amy-medium.onnx` |

Whisper uses a single layout:

| Component | Path |
|-----------|------|
| Binary    | `tools/whisper/Release/main.exe` |
| Model     | `tools/models/ggml-tiny.en.bin` |

## Tool Versions

Pinned versions are defined in `resources/tool-manifest.template.json`.
Run `scripts/fetch-tools.ps1` to download and verify all tools.

## Environment Override

Set `AI_INTERVIEWER_TOOLS` to override the tool directory location:

```
set AI_INTERVIEWER_TOOLS=C:\path\to\custom\tools
```

## Reproducible Builds

For CI and release builds:

1. `scripts/fetch-tools.ps1` downloads pinned versions with SHA-256 verification
2. `scripts/verify-tools.ps1` validates the download before building
3. No large binaries are committed to the repository

## Clean Machine Smoke Test

After building on a clean machine:

```batch
scripts\smoke-test.bat
```

This verifies all tools are present and functional.
