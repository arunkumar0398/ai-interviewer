#Requires -Version 5.1
<#
.SYNOPSIS
    Downloads pinned tool binaries for the AI Interviewer project.

.DESCRIPTION
    Fetches tool binaries defined in tool-manifest.json, verifies SHA-256
    checksums, and extracts/copies them to the tools directory.

.PARAMETER ManifestPath
    Path to the tool manifest JSON file. Defaults to resources/tool-manifest.json.

.PARAMETER ToolsDir
    Root directory for tools. Defaults to ./tools.

.EXAMPLE
    .\scripts\fetch-tools.ps1
    .\scripts\fetch-tools.ps1 -ManifestPath resources\tool-manifest.json -ToolsDir tools
#>
param(
    [string]$ManifestPath = "resources\tool-manifest.json",
    [string]$ToolsDir = "tools"
)

$ErrorActionPreference = "Stop"

function Write-Status {
    param([string]$Message, [string]$Color = "Cyan")
    Write-Host "  $Message" -ForegroundColor $Color
}

function Write-Success {
    param([string]$Message)
    Write-Host "  [OK] $Message" -ForegroundColor Green
}

function Write-Fail {
    param([string]$Message)
    Write-Host "  [FAIL] $Message" -ForegroundColor Red
}

function Get-SHA256 {
    param([string]$FilePath)
    $hash = Get-FileHash -Path $FilePath -Algorithm SHA256
    return $hash.Hash.ToLower()
}

function Test-FileChecksum {
    param(
        [string]$FilePath,
        [string]$ExpectedHash
    )
    $actual = Get-SHA256 -FilePath $FilePath
    if ($actual -eq $ExpectedHash.ToLower()) {
        return $true
    } else {
        Write-Status "Expected: $ExpectedHash" "Red"
        Write-Status "Actual:   $actual" "Red"
        return $false
    }
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "======================================" -ForegroundColor Cyan
Write-Host " AI Interviewer - Tool Fetcher" -ForegroundColor Cyan
Write-Host "======================================" -ForegroundColor Cyan
Write-Host ""

# Load manifest
if (-not (Test-Path $ManifestPath)) {
    Write-Host "ERROR: Manifest not found at $ManifestPath" -ForegroundColor Red
    exit 1
}

$manifest = Get-Content $ManifestPath -Raw | ConvertFrom-Json
Write-Status "Manifest: $ManifestPath"
Write-Status "Tools:    $ToolsDir"
Write-Host ""

$tempDir = Join-Path $env:TEMP "ai-interviewer-tools-$(Get-Random)"
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

$pass = 0
$fail = 0

foreach ($tool in $manifest.tools.PSObject.Properties) {
    $name = $tool.Name
    $config = $tool.Value
    $toolType = $config.type

    Write-Host "[$name]" -ForegroundColor Yellow
    Write-Status "Type:    $toolType"
    Write-Status "Version: $($config.version)"
    Write-Status "URL:     $($config.url)"

    $filename = Split-Path $config.url -Leaf
    $downloadPath = Join-Path $tempDir $filename
    $destination = Join-Path $ToolsDir $config.destination

    try {
        # Download
        Write-Status "Downloading..."
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $config.url -OutFile $downloadPath -UseBasicParsing

        # Verify checksum — always required
        if ($config.sha256) {
            if (Test-FileChecksum -FilePath $downloadPath -ExpectedHash $config.sha256) {
                Write-Success "Checksum verified"
            } else {
                Write-Fail "Checksum mismatch"
                $fail++
                Write-Host ""
                continue
            }
        } else {
            Write-Fail "No sha256 defined in manifest — cannot verify"
            $fail++
            Write-Host ""
            continue
        }

        # Install based on type
        if ($toolType -eq "archive") {
            Write-Status "Extracting archive to $destination..."
            New-Item -ItemType Directory -Path $destination -Force | Out-Null
            Expand-Archive -Path $downloadPath -DestinationPath $destination -Force

            # Normalize: ensure canonical executable paths after extraction
            if ($name -eq "piper") {
                # The pinned archive extracts a nested piper/ folder containing
                # piper.exe, its runtime DLLs (espeak-ng.dll, piper_phonemize.dll,
                # onnxruntime.dll, onnxruntime_providers_shared.dll) and
                # espeak-ng-data/. Move the WHOLE runtime up so every companion
                # is colocated with the canonical executable — never just the exe.
                $nestedDir = Join-Path $destination "piper"
                if ((Test-Path $nestedDir -PathType Container) -and -not (Test-Path (Join-Path $destination "piper.exe") -PathType Leaf)) {
                    Get-ChildItem -Path $nestedDir -Force | ForEach-Object {
                        Move-Item -Path $_.FullName -Destination $destination -Force
                    }
                    Remove-Item -Path $nestedDir -Force -ErrorAction SilentlyContinue
                    Write-Status "Normalized piper runtime (exe + DLLs + data) to canonical path" "DarkYellow"
                }
                # Fallback for other archive layouts: find piper.exe and move its
                # sibling runtime files (DLLs, espeak-ng-data) up with it.
                $canonical = Join-Path $destination "piper.exe"
                if (-not (Test-Path $canonical -PathType Leaf)) {
                    $nested = Get-ChildItem -Path $destination -Filter "piper.exe" -Recurse -File | Select-Object -First 1
                    if ($nested) {
                        $nestedParent = $nested.Directory
                        Get-ChildItem -Path $nestedParent -Force | ForEach-Object {
                            Move-Item -Path $_.FullName -Destination $destination -Force
                        }
                        Remove-Item -Path $nestedParent -Force -ErrorAction SilentlyContinue
                        Write-Status "Normalized piper runtime to canonical path" "DarkYellow"
                    }
                }
                # Piper requires espeak-ng-data adjacent to the executable for
                # phonemization at runtime. The pinned archive ships it at the
                # zip root; if a layout ever nests it, move it up so the
                # canonical path is runnable.
                $espeakData = Join-Path $destination "espeak-ng-data"
                if (-not (Test-Path (Join-Path $espeakData "phontab") -PathType Leaf)) {
                    $nestedData = Get-ChildItem -Path $destination -Directory -Filter "espeak-ng-data" -Recurse | Select-Object -First 1
                    if ($nestedData) {
                        Copy-Item -Path $nestedData.FullName -Destination $espeakData -Recurse -Force
                        Write-Status "Normalized espeak-ng-data to canonical path" "DarkYellow"
                    }
                }
            } elseif ($name -eq "whisper") {
                $canonicalDir = Join-Path $destination "Release"
                $canonical = Join-Path $canonicalDir "main.exe"
                if (-not (Test-Path $canonical -PathType Leaf)) {
                    # Some ZIP layouts put main.exe at root — move to Release/ so
                    # no stranded duplicate remains at the archive root.
                    $rootMain = Join-Path $destination "main.exe"
                    if (Test-Path $rootMain -PathType Leaf) {
                        New-Item -ItemType Directory -Path $canonicalDir -Force | Out-Null
                        Move-Item -Path $rootMain -Destination $canonical -Force
                        Write-Status "Normalized main.exe to Release/main.exe" "DarkYellow"
                    } else {
                        # Search recursively
                        $nested = Get-ChildItem -Path $destination -Filter "main.exe" -Recurse -File | Select-Object -First 1
                        if ($nested) {
                            New-Item -ItemType Directory -Path $canonicalDir -Force | Out-Null
                            Copy-Item -Path $nested.FullName -Destination $canonical -Force
                            Write-Status "Normalized main.exe to Release/main.exe" "DarkYellow"
                        }
                    }
                }
                # Move any remaining root-level files (e.g. companion DLLs) into
                # Release/ so all runtime dependencies are adjacent to the
                # canonical executable — keeping the layout runnable.
                Get-ChildItem -Path $destination -File -Force | ForEach-Object {
                    Move-Item -Path $_.FullName -Destination $canonicalDir -Force
                }
            }
        } elseif ($toolType -eq "file") {
            Write-Status "Copying file to $destination..."
            $destDir = Split-Path $destination -Parent
            if ($destDir) {
                New-Item -ItemType Directory -Path $destDir -Force | Out-Null
            }
            Copy-Item -Path $downloadPath -Destination $destination -Force
        } else {
            Write-Fail "Unknown tool type: $toolType"
            $fail++
            Write-Host ""
            continue
        }

        Write-Success "Installed"
        $pass++
    } catch {
        Write-Fail "Error: $($_.Exception.Message)"
        $fail++
    }

    Write-Host ""
}

# Cleanup
Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue

# Summary
Write-Host "======================================" -ForegroundColor Cyan
Write-Host " Results" -ForegroundColor Cyan
Write-Host "======================================" -ForegroundColor Cyan
Write-Host "  Installed: $pass" -ForegroundColor Green
if ($fail -gt 0) {
    Write-Host "  Failed:    $fail" -ForegroundColor Red
    exit 1
} else {
    Write-Host "  All tools installed successfully" -ForegroundColor Green
    exit 0
}
