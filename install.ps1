# Installs the latest desk on Windows:
#   irm https://raw.githubusercontent.com/robin7331/desk-operator/main/install.ps1 | iex
#
# DESK_INSTALL_DIR  where desk goes (default: %LOCALAPPDATA%\Programs\desk)
# DESK_BASE_URL     where the release files are (default: the latest GitHub release)

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    if (-not [Environment]::Is64BitOperatingSystem) {
        throw 'desk install: no build for 32-bit Windows. To build from source, see the README'
    }

    $asset = 'desk-x86_64-pc-windows-msvc.zip'
    $base = if ($env:DESK_BASE_URL) { $env:DESK_BASE_URL } else { 'https://github.com/robin7331/desk-operator/releases/latest/download' }
    $dest = if ($env:DESK_INSTALL_DIR) { $env:DESK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\desk' }

    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    $tmp = Join-Path $dest ('.desk-install-' + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        Write-Host "Downloading $asset"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile "$tmp\$asset"
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile "$tmp\SHA256SUMS"

        $expected = Get-Content "$tmp\SHA256SUMS" | ForEach-Object {
            $fields = $_ -split '\s+'
            if ($fields.Count -ge 2 -and $fields[1].TrimStart('*') -eq $asset) { $fields[0] }
        }
        if (-not $expected) { throw "desk install: SHA256SUMS doesn't list $asset" }
        if ((Get-FileHash -Algorithm SHA256 "$tmp\$asset").Hash -ne $expected) {
            throw "desk install: checksum mismatch for $asset, nothing installed"
        }

        Expand-Archive -Path "$tmp\$asset" -DestinationPath "$tmp\unpacked"
        $version = & "$tmp\unpacked\desk.exe" --version
        if ($LASTEXITCODE -ne 0) { throw "desk install: the downloaded desk doesn't run on this system, nothing installed" }

        Move-Item -Force "$tmp\unpacked\desk.exe" "$dest\desk.exe"
    } finally {
        Remove-Item -Recurse -Force $tmp
    }
    Write-Host "Installed $version to $dest\desk.exe"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $dest)) {
        [Environment]::SetEnvironmentVariable('Path', "$dest;$userPath".TrimEnd(';'), 'User')
        $env:Path = "$dest;$env:Path"
        Write-Host "Added $dest to your PATH. Other open terminals need a restart to see it."
    }

    if ([Console]::IsInputRedirected) {
        Write-Host 'For the agent skills, run: desk skill install'
    } else {
        $answer = Read-Host 'Install the agent skills for Claude Code (~/.claude/skills)? [y/N]'
        if ($answer -match '^(y|yes)$') {
            & "$dest\desk.exe" skill install --global
        } else {
            Write-Host 'Later, or for another agent: desk skill install'
        }
    }
}
