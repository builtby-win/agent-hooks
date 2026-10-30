# agent-hooks installer for Windows: detects arch, downloads the release zip,
# verifies sha256, installs to ~\.agent-hooks\bin, and registers hooks.
#
#   irm https://github.com/builtby-win/agent-hooks/releases/latest/download/install.ps1 | iex
#
# Env: AGENT_HOOKS_VERSION=v0.12.0  pin a release (default: latest)
#      AGENT_HOOKS_HOME=...         install root (default: ~\.agent-hooks)
#      AGENT_HOOKS_NO_HOOKS=1       place the binary only
#      AGENT_HOOKS_MIN_VERSION=X.Y.Z  reuse the installed copy when it is at least this version
#      AGENT_HOOKS_JSON=1           print the self-install result as the last stdout line
$ErrorActionPreference = 'Stop'

$home_dir = if ($env:AGENT_HOOKS_HOME) { $env:AGENT_HOOKS_HOME } else { "$HOME\.agent-hooks" }
$siArgs = @('self-install')
if ($env:AGENT_HOOKS_NO_HOOKS) { $siArgs += '--no-hooks' }
if ($env:AGENT_HOOKS_MIN_VERSION) { $siArgs += @('--min-version', $env:AGENT_HOOKS_MIN_VERSION) }
if ($env:AGENT_HOOKS_JSON) { $siArgs += '--json' }

# Fast path: a new-enough copy is already installed; just (re)register hooks.
$existing = Join-Path $home_dir 'bin\agent-hooks.exe'
if ($env:AGENT_HOOKS_MIN_VERSION -and (Test-Path $existing)) {
  $have = ((& $existing --version) -split '\s+')[-1] -replace '-.*$', ''
  $satisfied = $false
  try {
    $satisfied = [version]$have -ge [version]($env:AGENT_HOOKS_MIN_VERSION -replace '-.*$', '')
  } catch { }
  if ($satisfied) {
    & $existing @siArgs
    if ($LASTEXITCODE -ne 0) { throw "self-install failed" }
    return
  }
}

$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$target = "$arch-pc-windows-msvc"
$asset = "agent-hooks-$target.zip"
$base = if ($env:AGENT_HOOKS_VERSION) {
  "https://github.com/builtby-win/agent-hooks/releases/download/$($env:AGENT_HOOKS_VERSION)"
} else {
  'https://github.com/builtby-win/agent-hooks/releases/latest/download'
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("agent-hooks-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Invoke-WebRequest "$base/$asset" -OutFile "$tmp\$asset"
  Invoke-WebRequest "$base/$asset.sha256" -OutFile "$tmp\$asset.sha256"
  $expected = (Get-Content "$tmp\$asset.sha256" -Raw).Trim().Split()[0].ToLower()
  $actual = (Get-FileHash "$tmp\$asset" -Algorithm SHA256).Hash.ToLower()
  if ($expected -ne $actual) { throw "checksum mismatch for $asset" }

  Expand-Archive "$tmp\$asset" -DestinationPath $tmp
  $exe = Join-Path $tmp "agent-hooks-$target\agent-hooks.exe"
  & $exe @siArgs
  if ($LASTEXITCODE -ne 0) { throw "self-install failed" }

  $dest = Join-Path $home_dir 'bin'
  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';') -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', "$dest;$userPath", 'User')
    Write-Host "agent-hooks: added $dest to your user PATH (restart your terminal)"
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
