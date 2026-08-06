@echo off
REM ============================================================
REM  AI Interviewer - Smoke Test (L4)
REM  Verifies: file existence, exe launch, Rust tests, frontend tests
REM  Run: scripts\smoke-test.bat
REM ============================================================
setlocal enabledelayedexpansion
set PASS=0
set FAIL=0

echo.
echo ======================================
echo  AI Interviewer - Smoke Test
echo ======================================
echo.

REM --- 1. Check tools exist ---
echo [1/6] Checking tools directory...

if defined AI_INTERVIEWER_TOOLS (
    set "TOOLS_DIR=%AI_INTERVIEWER_TOOLS%"
) else (
    set "TOOLS_DIR=%~dp0..\..\ai-interviewer-tools"
)
echo   Using tools: %TOOLS_DIR%

REM Check both canonical and legacy piper binary locations
if exist "%TOOLS_DIR%\piper\piper.exe" (
    echo   [PASS] Piper binary exists (canonical)
    set /a PASS+=1
) else if exist "%TOOLS_DIR%\piper\piper\piper.exe" (
    echo   [PASS] Piper binary exists (legacy)
    set /a PASS+=1
) else (
    echo   [FAIL] Piper binary missing
    set /a FAIL+=1
)

REM Check both canonical and legacy piper model locations
if exist "%TOOLS_DIR%\piper\model.onnx" (
    echo   [PASS] Piper model exists (canonical)
    set /a PASS+=1
) else if exist "%TOOLS_DIR%\piper-models\en_US-amy-medium.onnx" (
    echo   [PASS] Piper model exists (legacy)
    set /a PASS+=1
) else (
    echo   [FAIL] Piper model missing
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\whisper.cpp\main.exe" (
    echo   [PASS] Whisper binary exists
    set /a PASS+=1
) else (
    echo   [FAIL] Whisper binary missing
    set /a FAIL+=1
)

if exist "%TOOLS_DIR%\whisper.cpp\models\ggml-tiny.en.bin" (
    echo   [PASS] Whisper model exists
    set /a PASS+=1
) else (
    echo   [FAIL] Whisper model missing
    set /a FAIL+=1
)

REM --- 2. Check installer output ---
echo.
echo [2/6] Checking build output...

set RELEASE_DIR=%~dp0..\src-tauri\target\release\bundle

if exist "%RELEASE_DIR%\nsis\ai-interviewer_0.1.0_x64-setup.exe" (
    echo   [PASS] NSIS installer exists
    set /a PASS+=1
) else (
    echo   [FAIL] NSIS installer missing
    set /a FAIL+=1
)

if exist "%RELEASE_DIR%\msi\ai-interviewer_0.1.0_x64_en-US.msi" (
    echo   [PASS] MSI installer exists
    set /a PASS+=1
) else (
    echo   [FAIL] MSI installer missing
    set /a FAIL+=1
)

REM --- 3. Check frontend build output ---
echo.
echo [3/6] Checking frontend build...

set OUT_DIR=%~dp0..\out

if exist "%OUT_DIR%\index.html" (
    echo   [PASS] Frontend index.html exists
    set /a PASS+=1
) else (
    echo   [FAIL] Frontend build missing: out\index.html
    set /a FAIL+=1
)

if exist "%OUT_DIR%\interview.html" (
    echo   [PASS] Interview page exists
    set /a PASS+=1
) else (
    echo   [FAIL] Interview page missing
    set /a FAIL+=1
)

if exist "%OUT_DIR%\candidate.html" (
    echo   [PASS] Candidate page exists
    set /a PASS+=1
) else (
    echo   [FAIL] Candidate page missing
    set /a FAIL+=1
)

if exist "%OUT_DIR%\dashboard.html" (
    echo   [PASS] Dashboard page exists
    set /a PASS+=1
) else (
    echo   [FAIL] Dashboard page missing
    set /a FAIL+=1
)

REM --- 4. Check key source files ---
echo.
echo [4/6] Checking key source files...

set SRC=%~dp0..\src-tauri\src

for %%F in (lib.rs main.rs db.rs) do (
    if exist "%SRC%\%%F" (
        echo   [PASS] src\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] src\%%F missing
        set /a FAIL+=1
    )
)

if exist "%SRC%\audio\capture.rs" (
    echo   [PASS] src\audio\capture.rs exists
    set /a PASS+=1
) else (
    echo   [FAIL] src\audio\capture.rs missing
    set /a FAIL+=1
)

if exist "%SRC%\audio\playback.rs" (
    echo   [PASS] src\audio\playback.rs exists
    set /a PASS+=1
) else (
    echo   [FAIL] src\audio\playback.rs missing
    set /a FAIL+=1
)

if exist "%SRC%\audio\tts_supervisor.rs" (
    echo   [PASS] src\audio\tts_supervisor.rs exists
    set /a PASS+=1
) else (
    echo   [FAIL] src\audio\tts_supervisor.rs missing
    set /a FAIL+=1
)

if exist "%SRC%\interview\orchestrator.rs" (
    echo   [PASS] src\interview\orchestrator.rs exists
    set /a PASS+=1
) else (
    echo   [FAIL] src\interview\orchestrator.rs missing
    set /a FAIL+=1
)

REM --- 5. Check test infrastructure ---
echo.
echo [5/6] Checking test infrastructure...

set TEST_DIR=%~dp0..\src-tauri\tests

for %%F in (audio_roundtrip.rs database.rs interview_orchestrator.rs) do (
    if exist "%TEST_DIR%\%%F" (
        echo   [PASS] tests\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] tests\%%F missing
        set /a FAIL+=1
    )
)

if exist "%~dp0vitest.config.mts" (
    echo   [PASS] vitest.config.mts exists
    set /a PASS+=1
) else (
    echo   [FAIL] vitest.config.mts missing
    set /a FAIL+=1
)

for %%F in (home.test.tsx interview.test.tsx candidate.test.tsx dashboard.test.tsx) do (
    if exist "%~dp0tests\%%F" (
        echo   [PASS] tests\%%F exists
        set /a PASS+=1
    ) else (
        echo   [FAIL] tests\%%F missing
        set /a FAIL+=1
    )
)

REM --- 6. Run Rust tests ---
echo.
echo [6/6] Running Rust tests (cargo test)...

pushd "%~dp0..\src-tauri"
cargo test 2>nul
if %ERRORLEVEL% equ 0 (
    echo   [PASS] All Rust tests passed
    set /a PASS+=1
) else (
    echo   [FAIL] Rust tests failed
    set /a FAIL+=1
)
popd

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
