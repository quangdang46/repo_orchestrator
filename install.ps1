# install.ps1 — One-liner installer for ro (Repo Forge Orchestrator) on Windows.
#
# Usage (pipe-safe — no [CmdletBinding]/param so iex works):
#   irm "https://raw.githubusercontent.com/quangdang46/repo_orchestrator/main/install.ps1" | iex
#
# Environment knobs:
#   $env:RO_VERSION   Specific version tag (e.g. "v0.1.0").  Default: latest release
#   $env:RO_PREFIX    Install directory for ro.exe.          Default: $env:LOCALAPPDATA\Programs\ro
#
# Downloads the pre-built binary from GitHub Releases — no Rust toolchain
# required.  Only needs PowerShell 5+ and internet access.

& {
    $ErrorActionPreference = 'Stop'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    $GH_REPO = 'quangdang46/repo_orchestrator'

    function Write-Step([string]$Message) {
        Write-Host "==> $Message" -ForegroundColor Green
    }

    function Write-Warn([string]$Message) {
        Write-Host "==> $Message" -ForegroundColor Yellow
    }

    function Fail([string]$Message) {
        Write-Host "==> ERROR: $Message" -ForegroundColor Red
        throw $Message
    }

    # ── Resolve version tag ──────────────────────────────────────────────
    $tag = $env:RO_VERSION
    if (-not $tag) {
        Write-Step 'resolving latest release ...'
        try {
            $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$GH_REPO/releases/latest" -UseBasicParsing
            $tag = $release.tag_name
        } catch {
            Fail "could not fetch latest release from GitHub: $_"
        }
    }
    Write-Step "version: $tag"

    # ── Resolve install prefix ───────────────────────────────────────────
    $prefix = if ($env:RO_PREFIX) { $env:RO_PREFIX } else { Join-Path $env:LOCALAPPDATA 'Programs\ro' }
    if (-not (Test-Path $prefix)) {
        New-Item -ItemType Directory -Path $prefix -Force | Out-Null
    }

    # ── Detect target triple ─────────────────────────────────────────────
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        'AMD64' { $target = 'x86_64-pc-windows-msvc' }
        'ARM64' { Fail "Windows ARM64 is not yet a published target for ro. Build from source: cargo build --release" }
        default { Fail "Unsupported Windows architecture: $arch (expected AMD64)." }
    }
    Write-Step "target: $target"

    # ── Build download URL ───────────────────────────────────────────────
    $archiveName = "ro-$target.zip"
    $downloadUrl = "https://github.com/$GH_REPO/releases/download/$tag/$archiveName"

    Write-Step "downloading $downloadUrl ..."
    $zipPath = Join-Path $env:TEMP $archiveName
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $zipPath -UseBasicParsing
    } catch {
        Fail "download failed: $_`n  URL: $downloadUrl`n  Ensure a release for $tag exists with a Windows binary."
    }

    # ── Verify SHA-256 if checksum file is available ─────────────────────
    $sha256Url = "$downloadUrl.sha256"
    try {
        $resp = Invoke-WebRequest -Uri $sha256Url -UseBasicParsing
        # .Content may be a byte array or a string depending on PS version.
        $raw = if ($resp.Content -is [byte[]]) {
            [System.Text.Encoding]::UTF8.GetString($resp.Content)
        } else {
            $resp.Content
        }
        $expectedHash = $raw.Trim().Split(' ')[0].ToLower()
        $actualHash   = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLower()
        if ($actualHash -ne $expectedHash) {
            Remove-Item $zipPath -Force -ErrorAction SilentlyContinue
            Fail "SHA-256 mismatch: expected $expectedHash, got $actualHash"
        }
        Write-Step 'SHA-256 checksum verified'
    } catch {
        Write-Warn 'SHA-256 checksum file not available — skipping verification'
    }

    # ── Extract ro.exe ──────────────────────────────────────────────────
    Write-Step "extracting to $prefix ..."
    $extractDir = Join-Path $env:TEMP "ro-extract-$([guid]::NewGuid().ToString('N'))"
    try {
        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force

        $exe = Get-ChildItem -Path $extractDir -Filter 'ro.exe' -Recurse | Select-Object -First 1
        if (-not $exe) { Fail 'ro.exe not found inside the downloaded archive' }

        Copy-Item -Path $exe.FullName -Destination (Join-Path $prefix 'ro.exe') -Force
    } finally {
        Remove-Item $zipPath      -Force -ErrorAction SilentlyContinue
        Remove-Item $extractDir   -Recurse -Force -ErrorAction SilentlyContinue
    }

    $bin = Join-Path $prefix 'ro.exe'
    if (-not (Test-Path $bin)) {
        Fail "extraction finished but $bin is missing"
    }
    Write-Step "installed: $bin"

    # ── Ensure PREFIX is on PATH ─────────────────────────────────────────
    if (-not (Get-Command ro -ErrorAction SilentlyContinue)) {
        # Add to current session.
        $env:Path = "$prefix;$env:Path"

        # Persist for future sessions (User scope).
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($userPath -notlike "*$prefix*") {
            [Environment]::SetEnvironmentVariable('Path', "$prefix;$userPath", 'User')
            Write-Step "added $prefix to your User PATH (takes effect in new terminals)"
        }
    }

    Write-Step 'running ro version'
    & $bin --version

    Write-Host @'

Next steps:
  1. ro init                 # initialize config & state
  2. ro doctor               # verify the install
  3. ro add owner/repo       # track a repository
  4. ro sync                 # sync all tracked repos

Configuration lives at %LOCALAPPDATA%\ro\config.toml (run `ro init` first).
'@
}
