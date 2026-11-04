# Plugover for Windows, in PowerShell:
#   irm https://github.com/wiktorekdev/plugover/releases/latest/download/installer.ps1 | iex
# Downloads plugover.exe of this release, checks it, and runs `plugover setup`, which asks whether
# this computer is the relay or the host and asks Windows for administrator rights.
# Keep this file ASCII: `irm | iex` does not always decode anything else correctly.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$version = '__VERSION__'
$file = "plugover-$version-windows-x86_64.exe"
$sha = '__SHA_WINDOWS__'
$url = "https://github.com/wiktorekdev/plugover/releases/download/v$version/$file"

if (-not [Environment]::Is64BitOperatingSystem) { throw 'Plugover needs 64-bit Windows.' }

$exe = Join-Path $env:TEMP $file
Write-Host "Downloading Plugover $version..."
Invoke-WebRequest $url -OutFile $exe -UseBasicParsing
if ((Get-FileHash $exe -Algorithm SHA256).Hash.ToLower() -ne $sha) {
    Remove-Item $exe -Force
    throw 'The download does not match its checksum; nothing was installed.'
}
& $exe setup
