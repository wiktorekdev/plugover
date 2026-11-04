//! `plugover setup`, the installer. The release's installer.sh and installer.ps1 only download
//! this binary and run it. It asks whether this computer is the relay or the host, shows what is
//! missing, installs Plugover as a service, and also updates and uninstalls it.

#[cfg(not(windows))]
mod linux;
#[cfg(not(windows))]
use linux as os;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

use crate::{
    proto::VERSION,
    term::{ACCENT, BAD, BOLD, DIM, fail, ok, paint, warn},
};
use anstream::println;
use anyhow::{Context, Result, bail};
use crossterm::{
    cursor, event,
    style::{Attribute, Color, Print, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};
use std::{
    fs,
    io::{IsTerminal, Read, Write, stdin, stdout},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(clap::Args, Clone, Default)]
pub struct Args {
    /// Install the host (punktfunk runs here).
    #[arg(long, conflicts_with_all = ["relay", "uninstall"])]
    pub host: bool,
    /// Install the relay (the devices are plugged in here).
    #[arg(long, conflicts_with = "uninstall")]
    pub relay: bool,
    /// Remove Plugover from this computer.
    #[arg(long)]
    pub uninstall: bool,
    /// With --uninstall, also delete pairings and settings.
    #[arg(long, requires = "uninstall")]
    pub purge: bool,
    /// Network the relay's firewall lets hosts in from, such as 192.168.1.0/24.
    #[arg(long, value_name = "CIDR")]
    pub lan: Option<String>,
    /// Do not ask for confirmation.
    #[arg(long, short)]
    pub yes: bool,
    /// Wait for Enter before exiting (for a window that would close at once).
    #[arg(long, hide = true)]
    pub pause: bool,
    /// Set on the copy of setup that Windows started elevated.
    #[arg(long, hide = true)]
    pub elevated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host,
    Relay,
    Uninstall,
}

pub enum Mark {
    Ok,
    Todo,
    Warn,
}

pub struct Line(pub Mark, pub String, pub String);

pub fn line(mark: Mark, what: impl Into<String>, detail: impl Into<String>) -> Line {
    Line(mark, what.into(), detail.into())
}

/// What `detect` found on this computer.
pub struct Found {
    pub lines: Vec<Line>,
    /// punktfunk-host is here, so this is probably the host.
    pub punktfunk: bool,
    /// Installed versions.
    pub relay: Option<String>,
    pub host: Option<String>,
}

pub type Action = Box<dyn FnOnce() -> Result<()>>;

pub struct Plan {
    pub lines: Vec<Line>,
    pub steps: Vec<(String, Action)>,
    /// Some step needs root (Linux asks sudo for it up front).
    pub root: bool,
    /// Runs after the steps: pairing code, checks, next steps.
    pub after: Action,
}

pub struct Ctx {
    pub args: Args,
    pub interactive: bool,
}

impl Ctx {
    pub fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        if self.args.yes {
            return Ok(true);
        }
        if !self.interactive {
            bail!("there is no terminal to answer \"{question}\" on; pass --yes to go ahead");
        }
        let answer = prompt(&format!("{question} {}", paint(DIM, if default { "[Y/n]" } else { "[y/N]" })))?;
        Ok(match answer.trim().to_lowercase().as_str() {
            "" => default,
            a => a.starts_with('y'),
        })
    }

    pub fn ask(&self, question: &str, default: &str) -> Result<String> {
        if self.args.yes || !self.interactive {
            return Ok(default.into());
        }
        let answer = prompt(&format!("{question} {}", paint(DIM, format!("[{default}]"))))?;
        Ok(if answer.trim().is_empty() { default.into() } else { answer.trim().into() })
    }
}

fn prompt(text: &str) -> Result<String> {
    anstream::print!("  {text} ");
    stdout().flush()?;
    let mut line = String::new();
    stdin().read_line(&mut line)?;
    Ok(line)
}

pub fn run(args: Args) -> Result<()> {
    let pause = args.pause;
    let result = setup(args);
    if let Err(e) = &result {
        anstream::eprintln!("\n{} {e:#}", paint(BAD.bold(), "error:"));
    }
    if pause {
        let _ = prompt(&paint(DIM, "Press Enter to close this window."));
    }
    // Already reported; the caller only needs the exit code.
    result.map_err(|_| anyhow::anyhow!(Reported))
}

/// An error `run` printed itself.
#[derive(Debug)]
pub struct Reported;

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("setup failed")
    }
}

fn setup(args: Args) -> Result<()> {
    os::prepare(&args)?;
    let ctx = Ctx { interactive: stdin().is_terminal() && stdout().is_terminal(), args };
    start_log();
    banner();
    let found = os::detect()?;
    heading("This computer");
    print_lines(&found.lines);

    let role = match (ctx.args.host, ctx.args.relay, ctx.args.uninstall) {
        (true, _, _) => Role::Host,
        (_, true, _) => Role::Relay,
        (_, _, true) => Role::Uninstall,
        _ if !ctx.interactive => bail!("no terminal to ask on; pass --host, --relay or --uninstall"),
        _ => match choose(&found)? {
            Some(r) => r,
            None => return Ok(()),
        },
    };
    let plan = os::plan(role, &found, &ctx)?;
    heading("Plan");
    print_lines(&plan.lines);
    let go = if role == Role::Uninstall { "Uninstall Plugover?" } else { "Go ahead?" };
    println!();
    if !ctx.confirm(go, role != Role::Uninstall)? {
        println!("\n  Nothing was changed.");
        return Ok(());
    }
    if plan.root {
        os::get_root()?;
    }
    heading(if role == Role::Uninstall { "Removing" } else { "Installing" });
    for (title, action) in plan.steps {
        step(&title, action)?;
    }
    heading("Done");
    (plan.after)()
}

fn choose(found: &Found) -> Result<Option<Role>> {
    heading("What should this computer be?");
    let note = |installed: &Option<String>, suggested: bool| match installed {
        Some(_) => paint(ACCENT, "  installed"),
        None if suggested => paint(ACCENT, "  suggested"),
        None => String::new(),
    };
    let mut items = vec![
        (Some(Role::Host), "Host", format!("punktfunk runs here, the devices come to it{}", note(&found.host, found.punktfunk))),
        (Some(Role::Relay), "Relay", format!("the devices are plugged in here{}", note(&found.relay, !found.punktfunk))),
    ];
    if found.relay.is_some() || found.host.is_some() {
        items.push((Some(Role::Uninstall), "Uninstall", "remove Plugover from this computer".into()));
    }
    items.push((None, "Quit", "change nothing".into()));
    let default = if found.punktfunk || found.host.is_some() { 0 } else { 1 };
    let labels: Vec<(&str, &str)> = items.iter().map(|(_, l, d)| (*l, d.as_str())).collect();
    Ok(items[menu(&labels, default)?].0)
}

// --- Logs, report, update --------------------------------------------------------------------

/// The installed role to show logs for: the one asked for, else the host, else the relay.
fn installed_role(want: Option<Role>) -> Result<Role> {
    let (relay, host) = os::installed();
    match want {
        Some(Role::Relay) if relay => Ok(Role::Relay),
        Some(Role::Host) if host => Ok(Role::Host),
        Some(_) => bail!("that role is not installed here"),
        None if host => Ok(Role::Host),
        None if relay => Ok(Role::Relay),
        None => bail!("Plugover is not set up here; run `plugover setup`"),
    }
}

pub fn logs(want: Option<Role>, follow: bool, lines: usize) -> Result<()> {
    os::logs(installed_role(want)?, follow, lines)
}

/// Everything a bug report needs, as plain text: versions, the checks and recent logs.
pub fn report(endpoint: &Path, host_config: &Path, relay_state: &Path) {
    anstream::ColorChoice::Never.write_global();
    println!("Plugover report: paste it into the bug report. It holds host names and addresses, no keys.");
    println!();
    println!("plugover {VERSION} · {} · {}", os::os_name(), std::env::consts::ARCH);
    crate::doctor::checks(endpoint, host_config, relay_state);
    let (relay, host) = os::installed();
    for (installed, role, name) in [(host, Role::Host, "host"), (relay, Role::Relay, "relay")] {
        if installed {
            println!("\n--- {name} log, last 40 lines");
            if let Err(e) = os::logs(role, false, 40) {
                println!("(could not read it: {e:#})");
            }
        }
    }
}

const RELEASES: &str = "https://api.github.com/repos/wiktorekdev/plugover/releases/latest";
const DOWNLOADS: &str = "https://github.com/wiktorekdev/plugover/releases/download";

/// "0.10.1" > "0.9.3", compared as numbers.
fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parse(a) > parse(b)
}

fn curl() -> &'static str {
    if cfg!(windows) { "curl.exe" } else { "curl" }
}

/// This platform's file in a release.
fn asset(version: &str) -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => format!("plugover-{version}-windows-x86_64.exe"),
        (_, "arm") => format!("plugover-{version}-linux-armv7"),
        (_, arch) => format!("plugover-{version}-linux-{arch}"),
    }
}

/// Installs the newest release over whatever is set up here, keeping pairings and settings.
pub fn update(check_only: bool, yes: bool) -> Result<()> {
    let release: serde_json::Value = serde_json::from_str(&exec(Command::new(curl()).args(["-fsSL", "-H", "Accept: application/vnd.github+json", RELEASES]))?)
        .context("GitHub answered something unexpected")?;
    let latest = release["tag_name"].as_str().context("the latest release has no tag")?.trim_start_matches('v').to_string();
    if !newer(&latest, VERSION) {
        ok(&format!("Plugover {VERSION} is up to date"), "");
        return Ok(());
    }
    println!("  {} {}", paint(ACCENT.bold(), "•"), paint(BOLD, format!("Plugover {latest} is out (you have {VERSION})")));
    if check_only {
        println!("    {} run {}", paint(DIM, "→"), crate::term::cmd("plugover update"));
        return Ok(());
    }
    anyhow::ensure!(!os::is_packaged(), "this plugover came with your package manager; update it there, then run `plugover setup` again");
    let (relay, host) = os::installed();
    anyhow::ensure!(relay || host, "Plugover is not set up here; run `plugover setup`");
    let ctx = Ctx { interactive: stdin().is_terminal() && stdout().is_terminal(), args: Args { yes, ..Default::default() } };
    if !ctx.confirm(&format!("Install Plugover {latest}?"), true)? {
        return Ok(());
    }

    let dir = std::env::temp_dir().join(format!("plugover-{latest}"));
    fs::create_dir_all(&dir)?;
    let (file, sums) = (dir.join(asset(&latest)), dir.join("SHA256SUMS"));
    for (url, path) in [(format!("{DOWNLOADS}/v{latest}/{}", asset(&latest)), &file), (format!("{DOWNLOADS}/v{latest}/SHA256SUMS"), &sums)] {
        exec(Command::new(curl()).args(["-fsSL", "-o"]).arg(path).arg(&url)).with_context(|| format!("downloading {url}"))?;
    }
    let want = fs::read_to_string(&sums)?
        .lines()
        .find_map(|l| l.strip_suffix(&asset(&latest)).map(|h| h.trim().to_string()))
        .context("SHA256SUMS does not list this platform's file")?;
    check_sha256(&file, &want)?;
    #[cfg(unix)]
    fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    ok(&format!("Downloaded Plugover {latest}"), "checksum matches");

    // The new binary installs itself, the same way the installer would.
    for (installed, flag) in [(relay, "--relay"), (host, "--host")] {
        if installed {
            let status = Command::new(&file).args(["setup", flag, "--yes"]).status()?;
            anyhow::ensure!(status.success(), "setup {flag} of {latest} failed; see above");
        }
    }
    Ok(())
}

// --- Terminal --------------------------------------------------------------------------------

const LOGO: [&str; 6] = [
    "██████╗ ██╗     ██╗   ██╗ ██████╗  ██████╗ ██╗   ██╗███████╗██████╗ ",
    "██╔══██╗██║     ██║   ██║██╔════╝ ██╔═══██╗██║   ██║██╔════╝██╔══██╗",
    "██████╔╝██║     ██║   ██║██║  ███╗██║   ██║██║   ██║█████╗  ██████╔╝",
    "██╔═══╝ ██║     ██║   ██║██║   ██║██║   ██║╚██╗ ██╔╝██╔══╝  ██╔══██╗",
    "██║     ███████╗╚██████╔╝╚██████╔╝╚██████╔╝ ╚████╔╝ ███████╗██║  ██║",
    "╚═╝     ╚══════╝ ╚═════╝  ╚═════╝  ╚═════╝   ╚═══╝  ╚══════╝╚═╝  ╚═╝",
];
const TAGLINE: &str = "USB devices from another computer, for punktfunk";

/// Violet to pink across `width` columns; 256-color steps where truecolor is not known to work.
fn gradient(text: &str, width: usize) -> String {
    use anstyle::{Ansi256Color, RgbColor, Style};
    let truecolor = matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor" | "24bit")) || std::env::var_os("WT_SESSION").is_some();
    let steps = [141u8, 141, 177, 177, 213, 212, 211];
    let width = width.max(2) - 1;
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            if c == ' ' {
                return " ".to_string();
            }
            let i = i.min(width);
            let style = if truecolor {
                let mix = |a: i32, b: i32| (a + (b - a) * i as i32 / width as i32) as u8;
                Style::new().fg_color(Some(RgbColor(mix(167, 244), mix(139, 114), mix(250, 182)).into()))
            } else {
                Style::new().fg_color(Some(Ansi256Color(steps[6 * i / width]).into()))
            };
            paint(style, c)
        })
        .collect()
}

fn banner() {
    println!();
    let columns = terminal::size().map_or(80, |(w, _)| w);
    if columns >= 72 {
        for row in LOGO {
            println!("  {}", gradient(row, LOGO[0].chars().count()));
        }
        println!("  {}", paint(DIM, format!("setup {VERSION} · {TAGLINE}")));
    } else {
        println!("  {} {}", gradient("◆ plugover", 10), paint(DIM, format!("{VERSION} · {TAGLINE}")));
    }
}

pub fn heading(text: &str) {
    println!("\n{}", paint(BOLD, text));
}

pub fn print_lines(lines: &[Line]) {
    for Line(mark, what, detail) in lines {
        match mark {
            Mark::Ok => ok(what, detail),
            Mark::Todo => println!("  {} {what}{}", paint(ACCENT.bold(), "•"), crate::term::after(DIM, detail)),
            Mark::Warn => warn(what, detail, None),
        }
    }
}

/// Arrow keys (or j/k, or a digit) and Enter.
fn menu(items: &[(&str, &str)], default: usize) -> Result<usize> {
    let mut out = stdout();
    let n = items.len();
    let mut sel = default;
    terminal::enable_raw_mode()?;
    crossterm::execute!(out, cursor::Hide)?;
    let picked = loop {
        for (i, (label, desc)) in items.iter().enumerate() {
            crossterm::queue!(out, terminal::Clear(ClearType::CurrentLine))?;
            if i == sel {
                crossterm::queue!(
                    out,
                    SetForegroundColor(Color::AnsiValue(141)),
                    SetAttribute(Attribute::Bold),
                    Print(format!("  ❯ {label:<10}")),
                    SetAttribute(Attribute::Reset),
                    Print(format!(" {}\r\n", strip(desc))),
                )?;
            } else {
                crossterm::queue!(
                    out,
                    Print(format!("    {label:<10} ")),
                    SetAttribute(Attribute::Dim),
                    Print(format!("{}\r\n", strip(desc))),
                    SetAttribute(Attribute::Reset),
                )?;
            }
        }
        out.flush()?;
        use event::{Event, KeyCode, KeyEventKind, KeyModifiers};
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => sel = (sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => sel = (sel + 1) % n,
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < n => sel = c as usize - '1' as usize,
            KeyCode::Enter => break Ok(sel),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Err(anyhow::anyhow!("cancelled")),
            KeyCode::Esc | KeyCode::Char('q') => break Err(anyhow::anyhow!("cancelled")),
            _ => {}
        }
        crossterm::queue!(out, cursor::MoveUp(n as u16))?;
    };
    crossterm::execute!(out, cursor::Show)?;
    terminal::disable_raw_mode()?;
    picked
}

/// Menu descriptions carry the `paint`ed notes; crossterm draws its own colors.
fn strip(s: &str) -> String {
    anstream::adapter::strip_str(s).to_string()
}

/// Runs one install step with a spinner, then prints how it went.
fn step(title: &str, action: Action) -> Result<()> {
    log(&format!("== {title}"));
    let tty = stdout().is_terminal();
    let done = Arc::new(AtomicBool::new(false));
    let spinner = tty.then(|| {
        let (done, title) = (done.clone(), title.to_string());
        thread::spawn(move || {
            let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut out = stdout();
            let _ = crossterm::execute!(out, cursor::Hide);
            for i in 0.. {
                if done.load(Ordering::SeqCst) {
                    break;
                }
                let _ = crossterm::execute!(
                    out,
                    cursor::MoveToColumn(0),
                    terminal::Clear(ClearType::CurrentLine),
                    SetForegroundColor(Color::AnsiValue(141)),
                    Print(format!("  {} ", frames[i % frames.len()])),
                    SetAttribute(Attribute::Reset),
                    Print(&title),
                );
                thread::sleep(Duration::from_millis(80));
            }
            let _ = crossterm::execute!(out, cursor::MoveToColumn(0), terminal::Clear(ClearType::CurrentLine), cursor::Show);
        })
    });
    let result = action();
    done.store(true, Ordering::SeqCst);
    if let Some(s) = spinner {
        let _ = s.join();
    }
    match result {
        Ok(()) => {
            ok(title, "");
            Ok(())
        }
        Err(e) => {
            log(&format!("failed: {e:#}"));
            fail(title, "", Some(&format!("the full log is in {}", log_path().display())));
            Err(e)
        }
    }
}

// --- Commands ---------------------------------------------------------------------------------

static LOG: Mutex<Option<fs::File>> = Mutex::new(None);

pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("plugover-setup.log")
}

fn start_log() {
    *LOG.lock().unwrap() = fs::File::create(log_path()).ok();
}

pub fn log(text: &str) {
    if let Some(f) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{}", text.trim_end());
    }
}

/// Runs a command, logs everything it prints and returns its standard output. A failure carries
/// the last lines it printed.
pub fn exec(cmd: &mut Command) -> Result<String> {
    exec_with(cmd, None)
}

pub fn exec_with(cmd: &mut Command, input: Option<&[u8]>) -> Result<String> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    log(&format!("$ {cmd:?}"));
    let mut child = cmd
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {name}"))?;
    if let (Some(data), Some(mut pipe)) = (input, child.stdin.take()) {
        pipe.write_all(data)?;
    }
    let out = child.wait_with_output()?;
    let (so, se) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    log(&so);
    log(&se);
    if !out.status.success() {
        let tail: Vec<&str> = se.lines().chain(so.lines()).filter(|l| !l.trim().is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(3)..].join("; ");
        bail!("{name} failed ({}){}", out.status, if tail.is_empty() { String::new() } else { format!(": {tail}") });
    }
    Ok(so.into_owned())
}

/// The first line a command prints, or None if it cannot run.
pub fn probe(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().trim().to_string())
}

/// `plugover 0.4.0` -> `0.4.0`.
pub fn version_of(bin: &Path) -> Option<String> {
    probe(bin, &["--version"])?.split_whitespace().nth(1).map(str::to_string)
}

/// Waits up to five seconds for `ready`.
pub fn wait_for(what: &str, mut ready: impl FnMut() -> bool) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if ready() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("{what} did not answer within five seconds")
}

pub fn relay_answers() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], crate::proto::DEFAULT_PORT));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// The console plugin, built into release binaries by scripts/build.sh. Empty in a plain build.
pub const PLUGIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/plugin.tgz"));

pub fn plugin_file() -> String {
    format!("punktfunk-plugin-plugover-{VERSION}.tgz")
}

/// Whether `path` is the binary running now, which cannot (Windows) or need not be copied over.
pub fn is_me(path: &Path) -> bool {
    let me = std::env::current_exe().and_then(fs::canonicalize).ok();
    me.is_some() && fs::canonicalize(path).ok() == me
}

/// Checks a downloaded file against its pinned SHA-256.
pub fn check_sha256(path: &Path, want: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got = hex::encode(h.finalize());
    anyhow::ensure!(got == want, "{} does not match its checksum (got {got}); nothing from it was installed", path.display());
    Ok(())
}

pub fn next_steps_host() {
    let paired = crate::hostd::call(&crate::endpoint(), &crate::hostd::Request::Status)
        .is_ok_and(|v| v["relays"].as_array().is_some_and(|r| !r.is_empty()));
    let what = if paired { "to pick the devices that connect when you stream" } else { "and pair with your relay" };
    println!("\n  {} open the punktfunk console, go to {} {what}.", paint(DIM, "Next:"), paint(BOLD, "Plugover"));
}
