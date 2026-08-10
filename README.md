# AI Interviewer

AI Interviewer is a Windows desktop workstation for running structured, local interviews with offline text-to-speech, microphone capture, and Whisper transcription.

## Run the portable workstation offline

1. Extract the entire portable ZIP to a writable folder. Do not run the executable from inside the ZIP.
2. Keep `ai-interviewer.exe`, `out/`, and `tools/` together in the extracted folder.
3. Run `ai-interviewer.exe`. Windows may ask for microphone permission on first launch.
4. Use the home-page audio test before starting an interview. The packaged Piper and Whisper tools run locally; an internet connection is not required.

If startup reports a missing tool, re-extract the complete ZIP. Antivirus quarantine or copying only the executable can leave the workstation incomplete.

## Interview data

Portable describes how the application is distributed; interview data is deliberately stored outside the extracted program folder in Tauri's Windows application-data directory (normally `%APPDATA%\com.ai-interviewer.app`). This keeps data across application upgrades and prevents it from being mixed with packaged tools.

The data directory contains:

- `interviews.db` — session and round records
- `recordings/` — persisted answer audio, grouped by session
- `tts/` — generated question audio
- `temp/` — temporary processing files

Closing or deleting the portable application folder does not delete these files. Back up or remove the application-data directory according to your organization's retention policy. Interview audio and transcripts may contain personal data; restrict access accordingly.

## Developer setup

Prerequisites are Node.js 22, stable Rust with the MSVC toolchain, PowerShell, and the Windows prerequisites for Tauri/WebView2.

```powershell
npm ci
pwsh scripts/fetch-tools.ps1 -ManifestPath resources/tool-manifest.json -ToolsDir tools
pwsh scripts/verify-tools.ps1 -ManifestPath resources/tool-manifest.json -ToolsDir tools
npm run tauri:dev
```

Frontend-only development is available with `npm run dev`. Before submitting changes, run:

```powershell
npm test
npm run lint -- --max-warnings=0
npx tsc --noEmit
Push-Location src-tauri
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
Pop-Location
```

Build the offline executable with `npx tauri build --no-bundle`. The Windows packaging workflow stages the static frontend, native executable, pinned local tools, notices, licenses, and this README into the portable ZIP. Tool layout and override details are documented in `docs/tools-layout.md`.

## Licenses

See `LICENSE`, `THIRD_PARTY_NOTICES`, and `licenses/` in the portable package. Public release remains blocked until the packaged voice model's redistribution terms are resolved as documented in `resources/licenses/model-attribution.txt`.
