<#
.SYNOPSIS
    Installs JumaBek on Windows.

.EXAMPLE
    irm https://raw.githubusercontent.com/jumabek-dev/jumabek/main/install.ps1 | iex

.EXAMPLE
    .\install.ps1 -Version v1.0.0 -Yes
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$Repo = "jumabek-dev/jumabek",
    [switch]$Yes
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Home_ = Join-Path $env:USERPROFILE ".jumabek"
$BinDir = Join-Path $Home_ "bin"
$SkillsDir = Join-Path $Home_ "skills"
$Target = "x86_64-pc-windows-msvc"

function Say($text) { Write-Host "  $text" }
function Step($text) { Write-Host "  $text" -ForegroundColor Cyan }
function Warn($text) { Write-Host "  $text" -ForegroundColor Yellow }
function Die($text) { Write-Host "  $text" -ForegroundColor Red; exit 1 }

function Confirm($question, $default = $true) {
    if ($Yes) { return $true }
    if (-not [Environment]::UserInteractive) { return $default }

    $suffix = if ($default) { "[Y/n]" } else { "[y/N]" }
    $answer = Read-Host "  $question $suffix"
    if ([string]::IsNullOrWhiteSpace($answer)) { return $default }
    return $answer -match '^(y|yes|д|да)$'
}

Write-Host ""
Write-Host "  JumaBek installer" -ForegroundColor Cyan
Write-Host ""

if (Get-Process jumabek -ErrorAction SilentlyContinue) {
    Die "JumaBek is running. Close it first - Windows will not let us replace a running binary."
}

# --- work out which release to fetch -----------------------------------------
if ($Version -eq "latest") {
    Step "looking up the latest release"
    try {
        $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    } catch {
        Die "cannot reach GitHub: $($_.Exception.Message)"
    }
    $Version = $release.tag_name
}

$asset = "jumabek-$Target.zip"
$url = "https://github.com/$Repo/releases/download/$Version/$asset"
Say "version $Version"

# --- download and verify -----------------------------------------------------
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("jumabek-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    Step "downloading $asset"
    $archive = Join-Path $work $asset
    try {
        Invoke-WebRequest -Uri $url -OutFile $archive
    } catch {
        Die "download failed: $url`n  is there a release for $Version and $Target?"
    }

    try {
        $expected = (Invoke-WebRequest -Uri "$url.sha256").Content.Split()[0].Trim()
        $actual = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
        if ($expected.ToLower() -ne $actual) {
            Die "checksum mismatch - the download is not what the release published"
        }
        Say "checksum ok"
    } catch {
        if ($_.Exception.Message -like "*checksum mismatch*") { throw }
        Warn "no checksum published, continuing without verification"
    }

    Step "unpacking"
    Expand-Archive -Path $archive -DestinationPath $work -Force
    $payload = Join-Path $work "jumabek-$Target"

    # --- install ------------------------------------------------------------
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    New-Item -ItemType Directory -Path $SkillsDir -Force | Out-Null

    Copy-Item (Join-Path $payload "jumabek.exe") $BinDir -Force
    Copy-Item (Join-Path $payload "shell_executor.exe") $SkillsDir -Force
    Step "installed to $BinDir"

    # Config and prompt belong to the user once they exist - never clobber them.
    foreach ($file in @("config.toml", "prompt.md", "secrets.toml.example")) {
        $source = Join-Path $payload $file
        $destination = Join-Path $Home_ $file
        if (-not (Test-Path $source)) { continue }

        if (Test-Path $destination) {
            Say "kept your existing $file"
        } else {
            Copy-Item $source $destination
            Say "created $file"
        }
    }
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}

# --- PATH --------------------------------------------------------------------
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -split ";" -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$BinDir", "User")
    Step "added $BinDir to your PATH - restart your terminal for it to take effect"
} else {
    Say "PATH already contains $BinDir"
}
$env:Path = "$env:Path;$BinDir"

# --- the LLM endpoint --------------------------------------------------------
Write-Host ""
Write-Host "  JumaBek needs an OpenAI-compatible endpoint to talk to." -ForegroundColor Cyan
Write-Host "  Point [llm].base_uri in $Home_\config.toml at whichever you use:"
Write-Host "    a local runner   Ollama, LM Studio, llama.cpp  (these want no API key)"
Write-Host "    a router         one endpoint in front of several providers"
Write-Host "    a provider       directly, with its own key"
Write-Host ""

# --- report ------------------------------------------------------------------
Write-Host ""
Step "checking the setup"
& (Join-Path $BinDir "jumabek.exe") doctor

Write-Host ""
Write-Host "  Set your API key, then run:  jumabek" -ForegroundColor Cyan
Write-Host "    An endpoint that wants an API key: setx JUMABEK_API_KEY ""your-key""" -ForegroundColor DarkGray
Write-Host "    or put it in $Home_\secrets.toml" -ForegroundColor DarkGray
Write-Host ""
