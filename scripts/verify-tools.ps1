#Requires -Version 5.1
<#
.SYNOPSIS
    Verifies tool binaries match expected SHA-256 checksums.

.DESCRIPTION
    Reads the tool manifest and verifies each tool binary exists and matches
    its expected checksum. Used in CI and before releases.

.PARAMETER ManifestPath
    Path to the tool manifest JSON file.

.PARAMETER ToolsDir
    Path to the tools directory to verify.

.EXAMPLE
    .\scripts\verify-tools.ps1
    .\scripts\verify-tools.ps1 -ToolsDir tools
#>
param(
    [string]$ManifestPath = "resources\tool-manifest.json",
    [string]$ToolsDir = "tools"
)

$ErrorActionPreference = "Stop"

Write-Host ""
Write-Host "======================================" -ForegroundColor Cyan
Write-Host " AI Interviewer - Tool Verifier" -ForegroundColor Cyan
Write-Host "======================================" -ForegroundColor Cyan
Write-Host ""

if (-not (Test-Path $ManifestPath)) {
    Write-Host "ERROR: Manifest not found at $ManifestPath" -ForegroundColor Red
    exit 1
}

$manifest = Get-Content $ManifestPath -Raw | ConvertFrom-Json
$pass = 0
$fail = 0
$skip = 0

foreach ($tool in $manifest.tools.PSObject.Properties) {
    $name = $tool.Name
    $config = $tool.Value

    $expectedPath = Join-Path $ToolsDir $config.extract_to

    Write-Host "[$name]" -ForegroundColor Yellow

    if (-not (Test-Path $expectedPath)) {
        # For directories, check if any files exist inside
        if (Test-Path $expectedPath -PathType Container) {
            $files = Get-ChildItem -Path $expectedPath -Recurse -File
            if ($files.Count -eq 0) {
                Write-Host "  [FAIL] Directory exists but is empty: $expectedPath" -ForegroundColor Red
                $fail++
                Write-Host ""
                continue
            }
            Write-Host "  [OK] Directory exists with $($files.Count) file(s)" -ForegroundColor Green
            $pass++
        } else {
            Write-Host "  [FAIL] Not found: $expectedPath" -ForegroundColor Red
            $fail++
        }
        Write-Host ""
        continue
    }

    # Check checksum if not a placeholder
    if ($config.sha256 -notlike "PLACEHOLDER_*") {
        $hash = (Get-FileHash -Path $expectedPath -Algorithm SHA256).Hash.ToLower()
        if ($hash -eq $config.sha256.ToLower()) {
            Write-Host "  [OK] Checksum verified: $($config.sha256)" -ForegroundColor Green
            $pass++
        } else {
            Write-Host "  [FAIL] Checksum mismatch" -ForegroundColor Red
            Write-Host "    Expected: $($config.sha256)" -ForegroundColor Red
            Write-Host "    Actual:   $hash" -ForegroundColor Red
            $fail++
        }
    } else {
        Write-Host "  [SKIP] Checksum is placeholder" -ForegroundColor Yellow
        $skip++
    }

    Write-Host ""
}

# Summary
Write-Host "======================================" -ForegroundColor Cyan
Write-Host " Results" -ForegroundColor Cyan
Write-Host "======================================" -ForegroundColor Cyan
Write-Host "  Passed:  $pass" -ForegroundColor Green
Write-Host "  Skipped: $skip" -ForegroundColor Yellow
if ($fail -gt 0) {
    Write-Host "  Failed:  $fail" -ForegroundColor Red
    exit 1
} else {
    Write-Host "  All tools verified" -ForegroundColor Green
    exit 0
}
