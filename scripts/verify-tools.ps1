#Requires -Version 5.1
<#
.SYNOPSIS
    Verifies tool binaries match expected SHA-256 checksums.

.DESCRIPTION
    Reads the tool manifest and verifies each tool exists at its destination
    and matches its expected checksum. Used in CI and before releases.

.PARAMETER ManifestPath
    Path to the tool manifest JSON file.

.PARAMETER ToolsDir
    Path to the tools root directory to verify.

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

foreach ($tool in $manifest.tools.PSObject.Properties) {
    $name = $tool.Name
    $config = $tool.Value
    $toolType = $config.type

    $expectedPath = Join-Path $ToolsDir $config.destination

    Write-Host "[$name]" -ForegroundColor Yellow
    Write-Host "  Type:    $toolType"

    if ($toolType -eq "archive") {
        # For archives, check canonical executable path exists
        if ($name -eq "piper") {
            $expectedExe = Join-Path $ToolsDir "piper\piper.exe"
            if (-not (Test-Path $expectedExe -PathType Leaf)) {
                Write-Host "  [FAIL] Canonical executable not found: $expectedExe" -ForegroundColor Red
                $fail++
            } else {
                Write-Host "  [OK] Canonical executable: piper\piper.exe" -ForegroundColor Green
                $pass++
            }
        } elseif ($name -eq "whisper") {
            $expectedExe = Join-Path $ToolsDir "whisper\Release\main.exe"
            if (-not (Test-Path $expectedExe -PathType Leaf)) {
                Write-Host "  [FAIL] Canonical executable not found: $expectedExe" -ForegroundColor Red
                $fail++
            } else {
                Write-Host "  [OK] Canonical executable: whisper\Release\main.exe" -ForegroundColor Green
                $pass++
            }
        } else {
            # Generic archive check
            if (-not (Test-Path $expectedPath -PathType Container)) {
                Write-Host "  [FAIL] Directory not found: $expectedPath" -ForegroundColor Red
                $fail++
            } else {
                $files = Get-ChildItem -Path $expectedPath -Recurse -File
                if ($files.Count -eq 0) {
                    Write-Host "  [FAIL] Directory exists but is empty: $expectedPath" -ForegroundColor Red
                    $fail++
                } else {
                    Write-Host "  [OK] Directory exists with $($files.Count) file(s)" -ForegroundColor Green
                    $pass++
                }
            }
        }
    } elseif ($toolType -eq "file") {
        # For files, check if the specific file exists and verify checksum
        if (-not (Test-Path $expectedPath -PathType Leaf)) {
            Write-Host "  [FAIL] File not found: $expectedPath" -ForegroundColor Red
            $fail++
            Write-Host ""
            continue
        }

        if ($config.sha256) {
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
            Write-Host "  [FAIL] No sha256 defined in manifest" -ForegroundColor Red
            $fail++
        }
    } else {
        Write-Host "  [FAIL] Unknown tool type: $toolType" -ForegroundColor Red
        $fail++
    }

    Write-Host ""
}

# Summary
Write-Host "======================================" -ForegroundColor Cyan
Write-Host " Results" -ForegroundColor Cyan
Write-Host "======================================" -ForegroundColor Cyan
Write-Host "  Passed:  $pass" -ForegroundColor Green
if ($fail -gt 0) {
    Write-Host "  Failed:  $fail" -ForegroundColor Red
    exit 1
} else {
    Write-Host "  All tools verified" -ForegroundColor Green
    exit 0
}
