#Requires -Version 5.1
<#
.SYNOPSIS
    Runtime smoke test for the packaged Piper TTS executable.

.DESCRIPTION
    Invokes the STAGED piper.exe (never repository-level tools/) against the
    staged model.onnx to synthesize a short phrase into a temporary WAV with a
    bounded timeout. Requires exit code 0 and a non-empty WAV, then cleans up.

.PARAMETER PiperDir
    Directory containing piper.exe, model.onnx, model.onnx.json, the runtime
    DLLs and espeak-ng-data. Defaults to portable\tools\piper.

.PARAMETER TimeoutSeconds
    Upper bound (seconds) for the synthesis. Default 60.

.EXAMPLE
    .\scripts\piper-runtime-smoke.ps1 -PiperDir portable\tools\piper
#>
param(
    [string]$PiperDir = "portable\tools\piper",
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = "Stop"

$exe = Join-Path $PiperDir "piper.exe"
$model = Join-Path $PiperDir "model.onnx"
$modelJson = Join-Path $PiperDir "model.onnx.json"

foreach ($p in @($exe, $model, $modelJson)) {
    if (-not (Test-Path $p -PathType Leaf)) {
        Write-Host "  [FAIL] Piper runtime smoke: missing $p" -ForegroundColor Red
        exit 1
    }
}

# The staged WAV must be cleaned up on every path.
$out = Join-Path $env:TEMP ("piper_smoke_" + [guid]::NewGuid().ToString("N") + ".wav")

try {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = "--model `"$model`" --output-file `"$out`""
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.WorkingDirectory = $PiperDir

    $proc = [System.Diagnostics.Process]::Start($psi)
    try {
        $proc.StandardInput.Write("This is a smoke test of the packaged Piper runtime.")
        $proc.StandardInput.Close()
    } catch {
        if (-not $proc.HasExited) { $proc.Kill() }
        Write-Host "  [FAIL] Piper runtime smoke: could not feed text: $($_.Exception.Message)" -ForegroundColor Red
        exit 1
    }

    if (-not $proc.WaitForExit($TimeoutSeconds * 1000)) {
        if (-not $proc.HasExited) { $proc.Kill() }
        Write-Host "  [FAIL] Piper runtime smoke: timed out after ${TimeoutSeconds}s" -ForegroundColor Red
        exit 1
    }

    if ($proc.ExitCode -ne 0) {
        Write-Host "  [FAIL] Piper runtime smoke: exit code $($proc.ExitCode)" -ForegroundColor Red
        exit 1
    }

    if (-not (Test-Path $out -PathType Leaf)) {
        Write-Host "  [FAIL] Piper runtime smoke: no WAV produced" -ForegroundColor Red
        exit 1
    }

    $len = (Get-Item $out).Length
    if ($len -le 44) {
        Write-Host "  [FAIL] Piper runtime smoke: WAV too small ($len bytes)" -ForegroundColor Red
        exit 1
    }

    Write-Host "  [OK] Piper runtime smoke: synthesized WAV ($len bytes)" -ForegroundColor Green
    exit 0
} finally {
    Remove-Item -Path $out -Force -ErrorAction SilentlyContinue
}
