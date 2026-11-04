//! Setup on Windows: the USB/IP drivers, scheduled tasks that run Plugover as SYSTEM at boot, the
//! firewall and folder permissions. Windows' own tools do the work (PowerShell, msiexec, curl.exe);
//! setup runs elevated and relaunches itself that way when it is not.

use super::*;
use crate::term::{GOOD, WARN, cmd};
use anstream::println;

/// usbipd-win serves the relay's devices.
const USBIPD_VERSION: &str = "5.3.0";
const USBIPD_SHA: &str = "1c984914aec944de19b64eff232421439629699f8138e3ddc29301175bc6d938";
/// usbip-win2 is the signed driver that plugs devices in on the host.
const USBIP_VERSION: &str = "0.9.8.0";
const USBIP_SHA: &str = "81f426741f7ee2ed991febe24a22daca8400b6ae2f171054e3fb404897e15d39";
const RELAY_TASK: &str = "Plugover relay";
const HOST_TASK: &str = "Plugover host";

fn install_dir() -> PathBuf {
    std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| r"C:\Program Files".into()).join("Plugover")
}

fn installed_exe() -> PathBuf {
    install_dir().join("plugover.exe")
}

/// Runs a PowerShell script. Passed encoded, so no quoting survives the command line by luck.
fn ps(script: &str) -> Result<String> {
    let utf16: Vec<u8> = format!("$ErrorActionPreference = 'Stop'\n$ProgressPreference = 'SilentlyContinue'\n{script}")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    exec(Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &base64(&utf16)]))
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// A PowerShell single-quoted string.
fn q(s: impl AsRef<std::ffi::OsStr>) -> String {
    format!("'{}'", s.as_ref().to_string_lossy().replace('\'', "''"))
}

/// An elevated process carries the High integrity level (S-1-16-12288) among its groups.
fn is_admin() -> bool {
    probe_all("whoami", &["/groups"]).is_some_and(|g| g.contains("S-1-16-12288"))
}

fn probe_all(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Relaunches setup elevated in its own window when it is not.
pub fn prepare(args: &Args) -> Result<()> {
    if is_admin() {
        return Ok(());
    }
    // Relaunched already and still not elevated: asking again would loop.
    anyhow::ensure!(!args.elevated, "setup still has no administrator rights; run it from an administrator terminal");
    let me = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        args.push("setup".into());
    }
    args.extend(["--pause".into(), "--elevated".into()]);
    println!("\n  {}", paint(DIM, "Setup needs administrator rights; Windows asks for them now."));
    let list = args.iter().map(q).collect::<Vec<_>>().join(",");
    ps(&format!("Start-Process -Verb RunAs -FilePath {} -ArgumentList {list}", q(&me)))
        .context("Windows did not give administrator rights")?;
    println!("  {}", paint(DIM, "Setup continues in the new window."));
    std::process::exit(0);
}

pub fn get_root() -> Result<()> {
    Ok(())
}

pub fn os_name() -> String {
    probe("cmd", &["/c", "ver"]).filter(|v| !v.is_empty()).unwrap_or_else(|| "Windows".into())
}

fn task_exists(name: &str) -> bool {
    Command::new("schtasks").args(["/Query", "/TN", name]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Which roles are installed here.
pub fn installed() -> (bool, bool) {
    (task_exists(RELAY_TASK), task_exists(HOST_TASK))
}

pub fn is_packaged() -> bool {
    false
}

/// Prints the end of the task's log file, and with `follow` whatever it writes next.
pub fn logs(role: Role, follow: bool, lines: usize) -> Result<()> {
    let path = crate::program_data().join(if role == Role::Relay { "relay.log" } else { "host.log" });
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let all: Vec<&str> = text.lines().collect();
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("{l}");
    }
    if !follow {
        return Ok(());
    }
    let mut seen = text.len() as u64;
    loop {
        thread::sleep(Duration::from_millis(500));
        let len = fs::metadata(&path).map_or(seen, |m| m.len());
        if len > seen {
            let mut f = fs::File::open(&path)?;
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(seen))?;
            let mut more = String::new();
            f.read_to_string(&mut more)?;
            anstream::print!("{more}");
        }
        seen = len;
    }
}

/// One PowerShell run that collects everything `detect` needs, as key=value lines.
const DETECT: &str = r#"
$v = [Environment]::OSVersion.Version
"build=$($v.Build)"
"os=$((Get-CimInstance Win32_OperatingSystem).Caption)"
$u = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
    Where-Object { $_.DisplayName -like 'USBip*' } | Select-Object -First 1
if ($u) { "usbip=$($u.DisplayVersion)" }
$pf = (Get-Command punktfunk-host -ErrorAction SilentlyContinue).Source
if (-not $pf) {
    $pf = Get-ChildItem $env:ProgramFiles -Filter punktfunk-host.exe -Recurse -Depth 2 -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}
if ($pf) { "punktfunk=$pf" }
foreach ($t in 'Plugover relay', 'Plugover host') {
    if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) { "task=$t" }
}
"#;

struct Detected {
    build: u32,
    os: String,
    usbip: Option<String>,
    usbipd: Option<String>,
    punktfunk: Option<PathBuf>,
    relay_task: bool,
    host_task: bool,
}

fn detected() -> Result<Detected> {
    let out = ps(DETECT)?;
    let get = |key: &str| out.lines().find_map(|l| l.strip_prefix(&format!("{key}=")).map(|v| v.trim().to_string()));
    let tasks: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("task=")).map(str::trim).collect();
    let usbipd = crate::usbipd::exe();
    Ok(Detected {
        build: get("build").and_then(|b| b.parse().ok()).unwrap_or(0),
        os: get("os").unwrap_or_else(|| "Windows".into()),
        usbip: get("usbip"),
        usbipd: usbipd.exists().then(|| probe(&usbipd, &["--version"])).flatten().map(|v| leading_version(&v)),
        punktfunk: get("punktfunk").map(PathBuf::from),
        relay_task: tasks.contains(&RELAY_TASK),
        host_task: tasks.contains(&HOST_TASK),
    })
}

/// "5.3.0+42.Branch.main" -> "5.3.0".
fn leading_version(s: &str) -> String {
    s.split(|c: char| !(c.is_ascii_digit() || c == '.')).find(|p| !p.is_empty()).unwrap_or_default().trim_matches('.').to_string()
}

fn older(have: Option<&str>, want: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    have.is_none_or(|h| parse(&leading_version(h)) < parse(want))
}

pub fn detect() -> Result<Found> {
    let d = detected()?;
    anyhow::ensure!(d.build >= 18362, "Plugover needs 64-bit Windows 10 1903 or newer");
    let version = |task: bool| task.then(|| version_of(&installed_exe())).flatten().or(task.then(|| "?".into()));
    let (relay, host) = (version(d.relay_task), version(d.host_task));
    let mut lines = vec![line(Mark::Ok, d.os.clone(), format!("build {}", d.build))];
    lines.push(match &d.punktfunk {
        Some(_) => line(Mark::Ok, "punktfunk-host", "found"),
        None => line(Mark::Todo, "punktfunk-host", "not found"),
    });
    if let Some(v) = &d.usbipd {
        lines.push(line(Mark::Ok, format!("usbipd-win {v}"), "for the relay"));
    }
    if let Some(v) = &d.usbip {
        lines.push(line(Mark::Ok, format!("usbip-win2 {v}"), "for the host"));
    }
    for (role, v) in [("relay", &relay), ("host", &host)] {
        if let Some(v) = v {
            lines.push(line(Mark::Ok, format!("Plugover {role} {v}"), "installed"));
        }
    }
    if relay.is_none() && host.is_none() {
        lines.push(line(Mark::Todo, "Plugover", "not installed"));
    }
    Ok(Found { lines, punktfunk: d.punktfunk.is_some(), relay, host })
}

pub fn plan(role: Role, found: &Found, ctx: &Ctx) -> Result<Plan> {
    let d = detected()?;
    match role {
        Role::Relay => plan_relay(found, &d, ctx),
        Role::Host => plan_host(found, &d),
        Role::Uninstall => plan_uninstall(found, ctx),
    }
}

fn install_line(installed: &Option<String>, what: &str) -> Line {
    match installed {
        Some(v) if v == VERSION => line(Mark::Todo, format!("Reinstall the {what} {VERSION}"), "already this version"),
        Some(v) => line(Mark::Todo, format!("Update the {what}"), format!("{v} → {VERSION}")),
        None => line(Mark::Todo, format!("Install the {what} {VERSION}"), format!("{}, starts with Windows", install_dir().display())),
    }
}

/// Downloads with Windows' own curl.exe and checks the pinned hash.
fn download(url: &str, sha: &str) -> Result<PathBuf> {
    let name = url.rsplit('/').next().unwrap_or("download");
    let path = std::env::temp_dir().join(name);
    exec(Command::new("curl.exe").args(["-fsSL", "--retry", "2", "-o"]).arg(&path).arg(url))?;
    check_sha256(&path, sha)?;
    Ok(path)
}

/// Set when an installed driver needs Windows restarted.
static RESTART: AtomicBool = AtomicBool::new(false);

fn install_usbipd() -> Result<()> {
    let url = format!("https://github.com/dorssel/usbipd-win/releases/download/v{USBIPD_VERSION}/usbipd-win_{USBIPD_VERSION}_x64.msi");
    let msi = download(&url, USBIPD_SHA)?;
    let status = Command::new("msiexec.exe").arg("/i").arg(&msi).args(["/qn", "/norestart"]).status()?;
    match status.code() {
        Some(0) => {}
        Some(3010) => RESTART.store(true, Ordering::SeqCst),
        code => bail!("the usbipd-win installer failed (exit code {code:?})"),
    }
    anyhow::ensure!(crate::usbipd::exe().exists(), "usbipd.exe is not where it should be after installing usbipd-win");
    Ok(())
}

fn install_usbip() -> Result<()> {
    let url = format!("https://github.com/vadimgrn/usbip-win2/releases/download/v.{USBIP_VERSION}/USBip-{USBIP_VERSION}-x64.exe");
    let installer = download(&url, USBIP_SHA)?;
    let status = Command::new(&installer).args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-", "/COMPONENTS=main,client"]).status()?;
    anyhow::ensure!(status.success(), "the usbip-win2 installer failed (exit code {:?})", status.code());
    RESTART.store(true, Ordering::SeqCst);
    anyhow::ensure!(crate::vhci::usbip_exe().exists(), "usbip.exe is not where it should be after installing usbip-win2");
    Ok(())
}

/// Stops whatever runs the old binary, then copies this one into Program Files.
fn install_self() -> Result<()> {
    ps(&format!(
        "foreach ($t in {}, {}) {{ Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue }}
         Get-Process plugover -ErrorAction SilentlyContinue | Where-Object Id -ne {} | Stop-Process -Force
         Start-Sleep -Milliseconds 500",
        q(RELAY_TASK),
        q(HOST_TASK),
        std::process::id()
    ))?;
    fs::create_dir_all(install_dir())?;
    if !is_me(&installed_exe()) {
        fs::copy(std::env::current_exe()?, installed_exe()).context("copying plugover.exe into Program Files")?;
    }
    // Keys stay readable by SYSTEM and administrators only; the punktfunk plugin runs as LOCAL
    // SERVICE and may read just the api folder with the host daemon's token.
    let data = crate::program_data();
    let api = data.join("api");
    fs::create_dir_all(&api)?;
    ps(&format!(
        "icacls {} /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
         icacls {} /grant '*S-1-5-19:(OI)(CI)RX' | Out-Null",
        q(&data),
        q(&api)
    ))
    .map(drop)
}

/// Runs `plugover <arg>` as SYSTEM at boot, restarting it if it stops, and starts it now.
fn register_task(name: &str, arg: &str, log: &str) -> Result<()> {
    let exe = installed_exe();
    let log = crate::program_data().join(log);
    ps(&format!(
        "$action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument ('/c \"\"' + {exe} + '\" {arg} >> \"' + {log} + '\" 2>&1\"')
         $trigger = New-ScheduledTaskTrigger -AtStartup
         $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable `
             -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
         $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
         Register-ScheduledTask -TaskName {name} -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
         Start-ScheduledTask -TaskName {name}",
        exe = q(&exe),
        log = q(&log),
        name = q(name),
    ))
    .map(drop)
}

fn firewall_rule(name: &str, rule: &str) -> String {
    format!(
        "Remove-NetFirewallRule -DisplayName {n} -ErrorAction SilentlyContinue
         New-NetFirewallRule -DisplayName {n} -Direction Inbound {rule} -Profile Any | Out-Null\n",
        n = q(name)
    )
}

// --- Relay -------------------------------------------------------------------------------------

fn plan_relay(found: &Found, d: &Detected, ctx: &Ctx) -> Result<Plan> {
    let mut lines = Vec::new();
    let need_usbipd = older(d.usbipd.as_deref(), USBIPD_VERSION);
    lines.push(if need_usbipd {
        line(Mark::Todo, format!("Install usbipd-win {USBIPD_VERSION}"), "serves the devices; from its GitHub release, checksum pinned")
    } else {
        line(Mark::Ok, "usbipd-win", "up to date")
    });
    lines.push(install_line(&found.relay, "relay"));
    lines.push(line(Mark::Todo, "Set up the firewall", "Plugover on TCP 7275 from the local network; usbipd's own port 3240 blocked"));
    let new_code = found.relay.is_none() || (ctx.interactive && !ctx.args.yes && ctx.confirm("Print a new pairing code at the end?", false)?);
    if new_code {
        lines.push(line(Mark::Todo, "Print a pairing code", "you type it on the host"));
    }

    let mut steps: Vec<(String, Action)> = Vec::new();
    if need_usbipd {
        steps.push((format!("Installing usbipd-win {USBIPD_VERSION}"), Box::new(install_usbipd)));
    }
    steps.push((format!("Installing plugover {VERSION}"), Box::new(install_self)));
    steps.push(("Setting up the firewall".into(), Box::new(relay_firewall)));
    steps.push(("Starting the relay".into(), Box::new(|| register_task(RELAY_TASK, "relay", "relay.log"))));
    steps.push(("Checking that the relay answers".into(), Box::new(|| wait_for("the relay", relay_answers))));
    let after: Action = Box::new(move || {
        restart_note("usbipd-win starts");
        if new_code {
            crate::cli::print_code(&crate::relay::open_pairing(&crate::relay_state())?);
            println!("\n  {}", paint(DIM, "For a new code later, run \"plugover code\" in an administrator terminal."));
        } else {
            ok("The relay is up to date", "pairings are kept");
        }
        println!("\n  {} on the host, open USB devices in the punktfunk console, or run {}.", paint(DIM, "Next:"), cmd("plugover pair"));
        Ok(())
    });
    Ok(Plan { lines, steps, root: true, after })
}

/// usbipd has no authentication of its own: anyone who reaches its port could skip pairing. Only
/// Plugover talks to it, over loopback, which Windows Firewall does not filter.
fn relay_firewall() -> Result<()> {
    let exe = q(installed_exe());
    let mut script = String::from("Get-NetFirewallRule -DisplayName 'usbipd' -ErrorAction SilentlyContinue | Disable-NetFirewallRule\n");
    script += &firewall_rule("Plugover (block usbipd)", "-Protocol TCP -LocalPort 3240 -Action Block");
    script += &firewall_rule("Plugover (mDNS)", &format!("-Program {exe} -Protocol UDP -LocalPort 5353 -Action Allow"));
    script += &firewall_rule("Plugover relay", &format!("-Program {exe} -Protocol TCP -LocalPort 7275 -RemoteAddress LocalSubnet -Action Allow"));
    script += "if ((Get-NetFirewallProfile | Where-Object { -not $_.Enabled }).Count -gt 0) { 'firewall-off' }";
    if ps(&script)?.contains("firewall-off") {
        warn("Windows Firewall is off for some networks", "there, usbipd's port 3240 is reachable without pairing", None);
    }
    Ok(())
}

fn restart_note(what: &str) {
    if RESTART.load(Ordering::SeqCst) {
        println!("  {} {}", paint(WARN.bold(), "!"), paint(WARN, format!("Restart Windows once so {what}.")));
    }
}

// --- Host --------------------------------------------------------------------------------------

fn plan_host(found: &Found, d: &Detected) -> Result<Plan> {
    let mut lines = Vec::new();
    let need_usbip = older(d.usbip.as_deref(), USBIP_VERSION);
    lines.push(if need_usbip {
        line(Mark::Todo, format!("Install usbip-win2 {USBIP_VERSION}"), "the USB/IP driver; from its GitHub release, checksum pinned")
    } else {
        line(Mark::Ok, "usbip-win2", "up to date")
    });
    lines.push(install_line(&found.host, "host"));
    let punktfunk = d.punktfunk.clone();
    lines.push(match (&punktfunk, PLUGIN.is_empty()) {
        (Some(_), false) => line(Mark::Todo, "Add the Plugover page to the punktfunk console", "and turn its plugin runner on"),
        (Some(_), true) => line(Mark::Warn, "This build has no console plugin inside", "release builds from scripts/build.sh do"),
        (None, _) => line(Mark::Warn, "No punktfunk-host here", "run setup again after installing it to add the console plugin"),
    });

    let mut steps: Vec<(String, Action)> = Vec::new();
    if need_usbip {
        steps.push((format!("Installing usbip-win2 {USBIP_VERSION}"), Box::new(install_usbip)));
    }
    steps.push((format!("Installing plugover {VERSION}"), Box::new(install_self)));
    steps.push((
        "Letting mDNS through the firewall".into(),
        Box::new(|| ps(&firewall_rule("Plugover (mDNS)", &format!("-Program {} -Protocol UDP -LocalPort 5353 -Action Allow", q(installed_exe())))).map(drop)),
    ));
    steps.push(("Starting the host daemon".into(), Box::new(|| register_task(HOST_TASK, "host", "host.log"))));
    if let Some(pf) = punktfunk.filter(|_| !PLUGIN.is_empty()) {
        steps.push(("Adding the console plugin".into(), Box::new(move || add_plugin(&pf))));
    }
    steps.push((
        "Checking that the host daemon answers".into(),
        Box::new(|| wait_for("the host daemon", || crate::hostd::call(&crate::endpoint(), &crate::hostd::Request::Status).is_ok())),
    ));
    let after: Action = Box::new(|| {
        restart_note("the USB/IP driver loads");
        crate::doctor::checks(&crate::endpoint(), &crate::config_dir().join("host.toml"), &crate::relay_state());
        next_steps_host();
        Ok(())
    });
    Ok(Plan { lines, steps, root: true, after })
}

fn add_plugin(punktfunk: &Path) -> Result<()> {
    // The runner re-resolves packages from inside its own tree, so the package lives there.
    let vendor = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| r"C:\ProgramData".into()).join(r"punktfunk\plugins\vendor");
    fs::create_dir_all(&vendor)?;
    let path = vendor.join(plugin_file());
    fs::write(&path, PLUGIN)?;
    exec(Command::new(punktfunk).args(["plugins", "add"]).arg(&path).arg("--allow-public-registry"))?;
    exec(Command::new(punktfunk).args(["plugins", "enable"])).map(drop)
}

// --- Uninstall ---------------------------------------------------------------------------------

fn plan_uninstall(found: &Found, ctx: &Ctx) -> Result<Plan> {
    anyhow::ensure!(found.relay.is_some() || found.host.is_some(), "Plugover is not installed here");
    let mut lines = Vec::new();
    for (role, v) in [("relay", &found.relay), ("host", &found.host)] {
        if v.is_some() {
            lines.push(line(Mark::Todo, format!("Remove the {role}"), "its task and firewall rules"));
        }
    }
    lines.push(line(Mark::Todo, "Remove plugover.exe", install_dir().display().to_string()));
    let purge = ctx.args.purge || (!ctx.args.yes && ctx.interactive && ctx.confirm("Also delete pairings and settings?", false)?);
    if purge {
        lines.push(line(Mark::Todo, "Delete pairings and settings", "you pair again after reinstalling"));
    }
    let steps: Vec<(String, Action)> = vec![("Removing Plugover".into(), Box::new(move || remove(purge)))];
    let after: Action = Box::new(|| {
        println!("  {}", paint(GOOD.bold(), "Plugover is gone."));
        println!("  {}", paint(DIM, "usbipd-win and usbip-win2 stay installed; remove them in Settings > Apps if nothing else uses them."));
        Ok(())
    });
    Ok(Plan { lines, steps, root: true, after })
}

fn remove(purge: bool) -> Result<()> {
    let mut script = format!(
        "foreach ($t in {r}, {h}) {{
             Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
             Unregister-ScheduledTask -TaskName $t -Confirm:$false -ErrorAction SilentlyContinue
         }}
         Get-Process plugover -ErrorAction SilentlyContinue | Where-Object Id -ne {me} | Stop-Process -Force
         foreach ($n in 'Plugover (block usbipd)', 'Plugover (mDNS)', 'Plugover relay') {{
             Remove-NetFirewallRule -DisplayName $n -ErrorAction SilentlyContinue
         }}
         $pf = (Get-Command punktfunk-host -ErrorAction SilentlyContinue).Source
         if ($pf) {{ & $pf plugins remove punktfunk-plugin-plugover 2>&1 | Out-Null }}
         Remove-Item (Join-Path $env:ProgramData 'punktfunk\\plugins\\vendor\\punktfunk-plugin-plugover-*.tgz') -ErrorAction SilentlyContinue\n",
        r = q(RELAY_TASK),
        h = q(HOST_TASK),
        me = std::process::id(),
    );
    if purge {
        script += &format!("Remove-Item -Recurse -Force {} -ErrorAction SilentlyContinue\n", q(crate::program_data()));
    }
    ps(&script)?;
    // A running program cannot delete itself; setup run from Program Files leaves just its exe.
    for e in fs::read_dir(install_dir()).into_iter().flatten().flatten() {
        if !is_me(&e.path()) {
            let _ = fs::remove_file(e.path());
        }
    }
    let _ = fs::remove_dir(install_dir());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn compares_versions() {
        assert_eq!(leading_version("5.3.0+42.Branch.main.Sha.abc"), "5.3.0");
        assert!(older(None, "5.3.0"));
        assert!(older(Some("5.2.0"), "5.3.0"));
        assert!(!older(Some("5.3.0"), "5.3.0"));
        assert!(!older(Some("0.9.10.0"), "0.9.8.0"));
    }
}
