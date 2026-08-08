@echo off
REM ============================================================
REM  AI Interviewer - Production Smoke Test
REM  Usage:
REM    scripts\smoke-test.bat                 - run all checks
REM    scripts\smoke-test.bat --artifact-only  - portable package checks only
REM ============================================================
setlocal enabledelayedexpansion
set PASS=0
set FAIL=0
set ARTIFACT_ONLY=0

if "%~1"=="--artifact-only" set ARTIFACT_ONLY=1

echo.
echo ======================================
echo  AI Interviewer - Production Smoke
if %ARTIFACT_ONLY% equ 1 (echo  Mode: artifact-only)
echo ======================================
echo.

if %ARTIFACT_ONLY% equ 1 goto :step8

REM --- 1. Check tool binaries ---
echo [1/8] Checking tool binaries...

set "TOOLS_DIR=%~dp0..\tools"

if exist "%TOOLS_DIR%\piper\piper.exe" (
    echo   [PASS] Piper binary exists
    set /a PASS+=1
) else (
    echo   [FAIL] Piper binary missing
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\piper\model.onnx" (
    echo   [PASS] Piper model exists
    set /a PASS+=1
) else (
    echo   [FAIL] Piper model missing
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\piper\model.onnx.json" (
    echo   [PASS] Piper model config exists
    set /a PASS+=1
) else (
    echo   [FAIL] Piper model config missing
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\whisper\Release\main.exe" (
    echo   [PASS] Whisper binary exists
    set /a PASS+=1
) else (
    echo   [FAIL] Whisper binary missing - expected at tools\whisper\Release\main.exe
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\models\ggml-tiny.en.bin" (
    echo   [PASS] Whisper model exists
    set /a PASS+=1
) else (
    echo   [FAIL] Whisper model missing
    set /a FAIL+=1
)

REM --- 2. Check tool manifest ---
echo.
echo [2/8] Checking tool manifest...

if exist "%~dp0..\resources\tool-manifest.json" (
    echo   [PASS] tool-manifest.json exists
    set /a PASS+=1
) else (
    echo   [FAIL] tool-manifest.json missing
    set /a FAIL+=1
)

REM --- 3. Check frontend build output ---
echo.
echo [3/8] Checking frontend build output...

set OUT_DIR=%~dp0..\out

for %%F in (index.html interview.html candidate.html dashboard.html) do (
    if exist "%OUT_DIR%\%%F" (
        echo   [PASS] out\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] out\%%F missing
        set /a FAIL+=1
    )
)

REM --- 4. Check key source files ---
echo.
echo [4/8] Checking key source files...

set SRC=%~dp0..\src-tauri\src

for %%F in (lib.rs main.rs paths.rs) do (
    if exist "%SRC%\%%F" (
        echo   [PASS] src\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] src\%%F missing
        set /a FAIL+=1
    )
)

for %%D in (audio interview) do (
    if exist "%SRC%\%%D" (
        echo   [PASS] src\%%D\ exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] src\%%D\ missing
        set /a FAIL+=1
    )
)

REM --- 5. Check test files ---
echo.
echo [5/8] Checking test files...

set TEST_DIR=%~dp0..\src-tauri\tests

for %%F in (audio_roundtrip.rs database.rs interview_orchestrator.rs command_contract.rs paths.rs) do (
    if exist "%TEST_DIR%\%%F" (
        echo   [PASS] tests\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] tests\%%F missing
        set /a FAIL+=1
    )
)

for %%F in (home.test.tsx interview.test.tsx candidate.test.tsx dashboard.test.tsx) do (
    if exist "%~dp0..\tests\%%F" (
        echo   [PASS] tests\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] tests\%%F missing
        set /a FAIL+=1
    )
)

REM --- 6. Run Rust quality gates ---
echo.
echo [6/8] Running Rust quality gates...

pushd "%~dp0..\src-tauri"

cargo fmt --all -- --check
if %ERRORLEVEL% equ 0 (
    echo   [PASS] cargo fmt
    set /a PASS+=1
) else (
    echo   [FAIL] cargo fmt
    set /a FAIL+=1
)

cargo clippy --locked --all-targets --all-features -- -D warnings
if %ERRORLEVEL% equ 0 (
    echo   [PASS] cargo clippy
    set /a PASS+=1
) else (
    echo   [FAIL] cargo clippy
    set /a FAIL+=1
)

cargo test --locked --all-targets --all-features
if %ERRORLEVEL% equ 0 (
    echo   [PASS] cargo test
    set /a PASS+=1
) else (
    echo   [FAIL] cargo test
    set /a FAIL+=1
)

cargo check --locked --features spike --bin audio-spike
if %ERRORLEVEL% equ 0 (
    echo   [PASS] cargo check audio-spike
    set /a PASS+=1
) else (
    echo   [FAIL] cargo check audio-spike
    set /a FAIL+=1
)

popd

REM --- 7. Run frontend quality gates ---
echo.
echo [7/8] Running frontend quality gates...

pushd "%~dp0.."

npm ci
if %ERRORLEVEL% equ 0 (
    echo   [PASS] npm ci
    set /a PASS+=1
) else (
    echo   [FAIL] npm ci
    set /a FAIL+=1
)

npm run lint -- --max-warnings=0
if %ERRORLEVEL% equ 0 (
    echo   [PASS] npm lint
    set /a PASS+=1
) else (
    echo   [FAIL] npm lint
    set /a FAIL+=1
)

npx tsc --noEmit
if %ERRORLEVEL% equ 0 (
    echo   [PASS] tsc --noEmit
    set /a PASS+=1
) else (
    echo   [FAIL] tsc --noEmit
    set /a FAIL+=1
)

npx vitest run
if %ERRORLEVEL% equ 0 (
    echo   [PASS] vitest run
    set /a PASS+=1
) else (
    echo   [FAIL] vitest run
    set /a FAIL+=1
)

npm run build
if %ERRORLEVEL% equ 0 (
    echo   [PASS] npm build
    set /a PASS+=1
) else (
    echo   [FAIL] npm build
    set /a FAIL+=1
)

popd

:step8
REM --- 8. Check portable package structure ---
echo.
echo [8/8] Checking portable package...

set "PORTABLE_DIR=%~dp0..\portable"
if exist "%PORTABLE_DIR%\ai-interviewer.exe" (
    echo   [PASS] Portable exe exists
    set /a PASS+=1
) else (
    echo   [FAIL] No portable directory - run tauri build first
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\LICENSE" (
    echo   [PASS] Portable LICENSE exists
    set /a PASS+=1
) else (
    echo   [FAIL] LICENSE missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\README.md" (
    echo   [PASS] Portable README.md exists
    set /a PASS+=1
) else (
    echo   [FAIL] README.md missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tool-manifest.json" (
    echo   [PASS] Portable tool-manifest.json exists
    set /a PASS+=1
) else (
    echo   [FAIL] tool-manifest.json missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\THIRD_PARTY_NOTICES" (
    echo   [PASS] Portable THIRD_PARTY_NOTICES exists
    set /a PASS+=1
) else (
    echo   [FAIL] THIRD_PARTY_NOTICES missing
    set /a FAIL+=1
)

REM --- License files (from resources/licenses/) ---
if exist "%PORTABLE_DIR%\licenses\Piper-LICENSE.txt" (
    echo   [PASS] Piper-LICENSE.txt exists
    set /a PASS+=1
) else (
    echo   [FAIL] Piper-LICENSE.txt missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\licenses\WhisperCPP-LICENSE.txt" (
    echo   [PASS] WhisperCPP-LICENSE.txt exists
    set /a PASS+=1
) else (
    echo   [FAIL] WhisperCPP-LICENSE.txt missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\licenses\model-attribution.txt" (
    echo   [PASS] model-attribution.txt exists
    set /a PASS+=1
) else (
    echo   [FAIL] model-attribution.txt missing
    set /a FAIL+=1
)

REM --- Required tool binaries in portable ---
if exist "%PORTABLE_DIR%\tools\piper\piper.exe" (
    echo   [PASS] Portable piper.exe exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable piper.exe missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tools\piper\model.onnx" (
    echo   [PASS] Portable piper model exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable piper model missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tools\piper\model.onnx.json" (
    echo   [PASS] Portable piper model config exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable piper model config missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tools\piper\espeak-ng-data\phontab" (
    echo   [PASS] Portable piper espeak-ng-data exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable piper espeak-ng-data missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tools\whisper\Release\main.exe" (
    echo   [PASS] Portable whisper binary exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable whisper binary missing
    set /a FAIL+=1
)

if exist "%PORTABLE_DIR%\tools\models\ggml-tiny.en.bin" (
    echo   [PASS] Portable whisper model exists
    set /a PASS+=1
) else (
    echo   [FAIL] Portable whisper model missing
    set /a FAIL+=1
)

REM --- Summary ---
echo.
echo ======================================
echo  Results
echo ======================================
echo   PASSED: %PASS%
echo   FAILED: %FAIL%
echo.

if %FAIL% equ 0 (
    echo   ALL CHECKS PASSED
    exit /b 0
) else (
    echo   SOME CHECKS FAILED
    exit /b 1
)
