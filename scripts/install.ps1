# zap-mcp installer for Windows.
#
#   irm https://github.com/don41382/zap-mcp/releases/latest/download/install.ps1 | iex
#
# Downloads the release archive, verifies it against the release's
# SHA256SUMS.txt and starts the guided setup, which installs zap-mcp to
# %LOCALAPPDATA%\Programs\zap-mcp (or offers update / uninstall if it's
# installed). Set $env:ZAP_MCP_VERSION = 'v0.3.0' to pick a specific release.

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'don41382/zap-mcp'
    $version = if ($env:ZAP_MCP_VERSION) { $env:ZAP_MCP_VERSION } else { 'latest' }
    $base = if ($env:ZAP_MCP_DOWNLOAD_URL) { $env:ZAP_MCP_DOWNLOAD_URL }
            elseif ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" }
            else { "https://github.com/$repo/releases/download/$version" }

    if (-not [Environment]::Is64BitOperatingSystem) { throw 'zap-mcp needs 64-bit Windows.' }
    # x64 build; also runs on Windows on ARM through emulation.
    $archive = 'zap-mcp-x86_64-pc-windows-msvc.zip'

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('zap-mcp-' + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $archive ..."
        Invoke-WebRequest "$base/$archive" -OutFile "$tmp\$archive" -UseBasicParsing
        Invoke-WebRequest "$base/SHA256SUMS.txt" -OutFile "$tmp\SHA256SUMS.txt" -UseBasicParsing

        $line = Get-Content "$tmp\SHA256SUMS.txt" | Where-Object { $_ -match (' ' + [regex]::Escape($archive) + '$') }
        $expected = if ($line) { ($line -split '\s+')[0].ToLowerInvariant() } else { '' }
        $actual = (Get-FileHash "$tmp\$archive" -Algorithm SHA256).Hash.ToLowerInvariant()
        if (-not $expected -or $actual -ne $expected) {
            throw 'Checksum verification failed; nothing was installed.'
        }
        Write-Host 'Checksum OK.'

        Expand-Archive "$tmp\$archive" -DestinationPath "$tmp\x" -Force
        $exe = Join-Path "$tmp\x" 'zap-mcp.exe'
        Unblock-File $exe
        & $exe setup --from-installer
    }
    finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}
