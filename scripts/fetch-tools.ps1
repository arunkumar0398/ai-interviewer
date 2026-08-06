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

.PARAMETER SkipChecksum
    Skip SHA-256 verification (for development only).

.EXAMPLE
    .\scripts\fetch-tools.ps1
    .\scripts\fetch-tools.ps1 -ManifestPath resources\tool-manifest.json -ToolsDir tools
#>
param(
    [string]$ManifestPath = "resources\tool-manifest.json",
    [string]$ToolsDir = "tools",
    [switch]$SkipChecksum
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
    if ($SkipChecksum) {
        Write-Status "Checksum verification skipped" "Yellow"
        return $true
    }
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

        # Verify checksum
        if ($config.sha256 -notlike "PLACEHOLDER_*") {
            if (Test-FileChecksum -FilePath $downloadPath -ExpectedHash $config.sha256) {
                Write-Success "Checksum verified"
            } else {
                Write-Fail "Checksum mismatch"
                $fail++
                Write-Host ""
                continue
            }
        } else {
            Write-Status "Checksum placeholder — skipping verification" "Yellow"
        }

        # Install based on type
        if ($toolType -eq "archive") {
            Write-Status "Extracting archive to $destination..."
            New-Item -ItemType Directory -Path $destination -Force | Out-Null
            Expand-Archive -Path $downloadPath -DestinationPath $destination -Force
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
