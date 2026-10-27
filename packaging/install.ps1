# Plugover installer for Windows. In PowerShell:
#   irm __BASE__/__SCRIPT__ | iex
# The host installer adds the usbip-win2 driver, the host daemon and the punktfunk plugin. The
# relay installer adds usbipd-win and the relay. Both run Plugover as a startup task under
# SYSTEM and are safe to run again to update.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Base = '__BASE__'
$Role = '__ROLE__'
$Script = '__SCRIPT__'
$PluginFile = 'punktfunk-plugin-plugover-__PLUGIN_VERSION__.tgz'
$UsbipFile = 'USBip-__USBIP_VERSION__-x64.exe'
$UsbipdFile = 'usbipd-win___USBIPD_VERSION___x64.msi'
# name = url, sha256
$Files = @{ 'plugover.exe' = "$Base/plugover.exe", '__SHA_EXE__' }
if ($Role -eq 'host') {
    $Files[$PluginFile] = "$Base/$PluginFile", '__SHA_PLUGIN__'
    $Files[$UsbipFile] = "$Base/$UsbipFile", '__SHA_USBIP__'
} else {
    $Files[$UsbipdFile] = 'https://github.com/dorssel/usbipd-win/releases/download/v__USBIPD_VERSION__/usbipd-win___USBIPD_VERSION___x64.msi', '__SHA_USBIPD__'
}

function Say($text) { Write-Host "==> $text" -ForegroundColor Cyan }

$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Say 'Asking for administrator rights...'
    Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -NoExit -Command `"irm $Base/$Script | iex`""
    return
}

if (-not [Environment]::Is64BitOperatingSystem -or [Environment]::OSVersion.Version.Build -lt 18362) {
    throw 'Plugover needs 64-bit Windows 10 1903 or newer.'
}

# Download everything first and check it, so a bad download changes nothing.
$tmp = Join-Path $env:TEMP 'plugover-install'
New-Item -ItemType Directory -Force $tmp | Out-Null
foreach ($name in $Files.Keys) {
    Say "Downloading $name"
    $dst = Join-Path $tmp $name
    Invoke-WebRequest $Files[$name][0] -OutFile $dst -UseBasicParsing
    if ((Get-FileHash $dst -Algorithm SHA256).Hash.ToLower() -ne $Files[$name][1]) {
        throw "$name does not match its checksum; nothing was installed."
    }
}

$restart = $false
$dir = Join-Path $env:ProgramFiles 'Plugover'
$exe = Join-Path $dir 'plugover.exe'
$data = Join-Path $env:ProgramData 'Plugover'
$api = Join-Path $data 'api'
$task = if ($Role -eq 'host') { 'Plugover host' } else { 'Plugover relay' }

if ($Role -eq 'host') {
# usbip-win2: the signed driver that plugs the relay's devices into Windows.
function Get-Usbip {
    Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -like 'USBip*' } | Select-Object -First 1
}
$usbipKey = Get-Usbip
$have = $null
if ($usbipKey) { try { $have = [version]($usbipKey.DisplayVersion -replace '[^0-9.]', '') } catch { } }
if (-not $have -or $have -lt [version]'__USBIP_VERSION__') {
    Say 'Installing the USB/IP driver (usbip-win2 __USBIP_VERSION__)'
    $p = Start-Process (Join-Path $tmp $UsbipFile) -Wait -PassThru -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/COMPONENTS="main,client"'
    if ($p.ExitCode -ne 0) { throw "The USB/IP driver installer failed (exit code $($p.ExitCode))." }
    $restart = $true
    $usbipKey = Get-Usbip
}
$candidates = @(Join-Path $env:ProgramFiles 'USBip\usbip.exe')
if ($usbipKey -and $usbipKey.InstallLocation) { $candidates += Join-Path $usbipKey.InstallLocation 'usbip.exe' }
$usbip = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $usbip) { throw 'usbip.exe was not found after installing usbip-win2.' }
[Environment]::SetEnvironmentVariable('PLUGOVER_USBIP', $usbip, 'Machine')
} else {
# usbipd-win serves the devices; Plugover's relay sits in front of it.
$usbipd = Join-Path $env:ProgramFiles 'usbipd-win\usbipd.exe'
$have = $null
if (Test-Path $usbipd) {
    try { $have = [version](((& $usbipd --version) -join ' ') -replace '^\D*(\d+\.\d+\.\d+).*$', '$1') } catch { }
}
if (-not $have -or $have -lt [version]'__USBIPD_VERSION__') {
    Say 'Installing usbipd-win __USBIPD_VERSION__'
    $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList '/i', "`"$(Join-Path $tmp $UsbipdFile)`"", '/qn', '/norestart'
    if ($p.ExitCode -notin 0, 3010) { throw "The usbipd-win installer failed (exit code $($p.ExitCode))." }
    if ($p.ExitCode -eq 3010) { $restart = $true }
}
if (-not (Test-Path $usbipd)) { throw 'usbipd.exe was not found after installing usbipd-win.' }
[Environment]::SetEnvironmentVariable('PLUGOVER_USBIPD', $usbipd, 'Machine')

# usbipd has no authentication of its own: anyone who reaches its port could skip pairing. Only
# Plugover talks to it, over loopback, which Windows Firewall does not filter.
Get-NetFirewallRule -DisplayName 'usbipd' -ErrorAction SilentlyContinue | Disable-NetFirewallRule
Remove-NetFirewallRule -DisplayName 'Plugover (block usbipd)' -ErrorAction SilentlyContinue
New-NetFirewallRule -DisplayName 'Plugover (block usbipd)' -Direction Inbound -Protocol TCP -LocalPort 3240 -Action Block -Profile Any | Out-Null
if ((Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count -gt 0) {
    Write-Warning 'Windows Firewall is off for some network profiles, so usbipd (TCP 3240) is reachable from the network there.'
}
}

Say "Installing the Plugover $Role"
Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
Get-Process plugover -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
New-Item -ItemType Directory -Force $dir, $api | Out-Null
Copy-Item (Join-Path $tmp 'plugover.exe') $exe -Force

# Keys stay readable by SYSTEM and administrators only; the punktfunk plugin runs as LOCAL
# SERVICE and may read just the api folder with the daemon's token.
icacls $data /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
icacls $api /grant '*S-1-5-19:(OI)(CI)RX' | Out-Null

Remove-NetFirewallRule -DisplayName 'Plugover (mDNS)' -ErrorAction SilentlyContinue
New-NetFirewallRule -DisplayName 'Plugover (mDNS)' -Direction Inbound -Program $exe -Protocol UDP -LocalPort 5353 -Action Allow -Profile Any | Out-Null
if ($Role -eq 'relay') {
    Remove-NetFirewallRule -DisplayName 'Plugover relay' -ErrorAction SilentlyContinue
    New-NetFirewallRule -DisplayName 'Plugover relay' -Direction Inbound -Program $exe -Protocol TCP -LocalPort 7275 -RemoteAddress LocalSubnet -Action Allow -Profile Any | Out-Null
}

$log = Join-Path $data "$Role.log"
$action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument "/c `"`"$exe`" $Role >> `"$log`" 2>&1`""
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
$principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
Register-ScheduledTask -TaskName $task -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $task

if ($Role -eq 'host') {
# The punktfunk plugin.
$pf = (Get-Command punktfunk-host -ErrorAction SilentlyContinue).Source
if (-not $pf) {
    $pf = Get-ChildItem $env:ProgramFiles -Filter punktfunk-host.exe -Recurse -Depth 2 -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}
if ($pf) {
    Say 'Adding the plugin to punktfunk'
    # The runner re-resolves packages from inside its own tree, so the package lives there.
    $vendor = Join-Path $env:ProgramData 'punktfunk\plugins\vendor'
    New-Item -ItemType Directory -Force $vendor | Out-Null
    Copy-Item (Join-Path $tmp $PluginFile) (Join-Path $vendor $PluginFile) -Force
    & $pf plugins add (Join-Path $vendor $PluginFile) --allow-public-registry
    if ($LASTEXITCODE -ne 0) { Write-Warning 'punktfunk could not add the plugin; see the output above.' }
    & $pf plugins enable | Out-Null
} else {
    Write-Warning 'punktfunk-host.exe was not found. Install the punktfunk host, then run this installer again.'
}
}

Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
Write-Host ''
Write-Host 'Plugover is installed.' -ForegroundColor Green
if ($restart) {
    Write-Host 'Restart Windows once so the new USB driver loads.' -ForegroundColor Yellow
}
if ($Role -eq 'host') {
    Write-Host 'Then open the punktfunk console, go to Plugins > USB devices, pick your relay and enter the code'
    Write-Host 'its installer printed.'
} else {
    Write-Host ''
    & $exe code
    Write-Host 'For a new code later, run "plugover code" in an administrator PowerShell.'
}
