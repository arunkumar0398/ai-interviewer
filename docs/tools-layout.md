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

## Runtime Integrity Contract

At application startup, the app verifies the files the manifest pins per-file
hashes for (entries with `type: "file"` in `resources/tool-manifest.json`):

- **Piper model** — SHA-256 verified at the SELECTED layout's path (canonical
  `tools/piper/model.onnx` or legacy `tools/piper-models/en_US-amy-medium.onnx`);
- **Piper model config** — SHA-256 verified at the SELECTED layout's path;
- **Whisper model** (`tools/models/ggml-tiny.en.bin`) — SHA-256 verified.

The verifier follows the same layout resolver the runtime uses
(`resolve_piper`), so a selected legacy runtime is hash-checked against its
actual active files, never only the canonical destinations.

Runtime executables and companion DLLs (`piper.exe`, `espeak-ng.dll`,
`piper_phonemize.dll`, `onnxruntime*.dll`, `espeak-ng-data/`, `main.exe`)
are **not** independently hashed at startup: the manifest stores archive-level
checksums for those packages, not per-file executable hashes. Their guarantees
are:

- coherent layout/existence validation at startup (readiness);
- source-archive SHA-256 verification during packaging
  (`scripts/fetch-tools.ps1` / `scripts/verify-tools.ps1`);
- a real staged Piper execution smoke test in Windows CI.

This is a deliberate, truthful scope: the app does not claim full executable
hash verification.
