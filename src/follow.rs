//! Streaming apps Plugover follows by watching them rather than through a hook: Wolf through its
//! API socket, Parsec and Steam Remote Play through their logs. The host daemon polls each one now
//! and then; `poll` says whether a stream runs.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Wolf,
    Parsec,
    Steam,
}

/// One followed app.
pub struct App {
    kind: Kind,
    /// The Wolf socket, or the log.
    pub path: PathBuf,
    /// How far the log is read.
    offset: u64,
    /// Parsec's connected clients; for Steam, whether a stream runs.
    clients: Vec<String>,
}

impl App {
    /// The source name its leases use.
    pub fn source(&self) -> &'static str {
        match self.kind {
            Kind::Wolf => "wolf",
            Kind::Parsec => "parsec",
            Kind::Steam => "steam",
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            Kind::Wolf => "Wolf",
            Kind::Parsec => "Parsec",
            Kind::Steam => "Steam Remote Play",
        }
    }

    /// Whether a stream runs now; `None` when the app cannot be asked (not running, no log).
    pub fn poll(&mut self) -> Option<bool> {
        match self.kind {
            Kind::Wolf => wolf_sessions(&self.path).ok().map(|n| n > 0),
            Kind::Parsec | Kind::Steam => {
                self.read().ok()?;
                // A crash writes no end line, so a stream also ends with the app.
                if !self.clients.is_empty() && !running(if self.kind == Kind::Parsec { "parsecd" } else { "steam" }) {
                    self.clients.clear();
                }
                Some(!self.clients.is_empty())
            }
        }
    }

    /// Reads what the log gained since the last poll. A log that shrank was started anew.
    fn read(&mut self) -> std::io::Result<()> {
        let mut f = fs::File::open(&self.path)?;
        let len = f.metadata()?.len();
        if len < self.offset {
            self.offset = 0;
            self.clients.clear();
        }
        f.seek(SeekFrom::Start(self.offset))?;
        let mut new = Vec::new();
        f.take(len - self.offset).read_to_end(&mut new)?;
        // Only whole lines; a half-written one waits for the next poll.
        let whole = new.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        self.offset += whole as u64;
        for line in String::from_utf8_lossy(&new[..whole]).lines() {
            self.line(line);
        }
        Ok(())
    }

    fn line(&mut self, line: &str) {
        let line = line.trim_end();
        match self.kind {
            // `[I 2024-03-08 11:12:10] name#123 connected.`, and a start line when parsecd starts.
            Kind::Parsec => {
                if line.contains("===== Parsec: Started =====") {
                    self.clients.clear();
                } else if let Some(who) = line.strip_suffix(" disconnected.").and_then(|l| l.split("] ").nth(1)) {
                    self.clients.retain(|c| c != who);
                } else if let Some(who) = line.strip_suffix(" connected.").and_then(|l| l.split("] ").nth(1))
                    && !self.clients.iter().any(|c| c == who)
                {
                    self.clients.push(who.to_string());
                }
            }
            // `[2026-10-15 12:00:00][1.0] Streaming started to Steam Deck at 192.168.1.6:1, ...`.
            // The end differs by capture: PipeWire on Linux, the encoder on Windows.
            Kind::Steam => {
                if line.contains("] Streaming started to ") {
                    self.clients = vec!["client".into()];
                } else if line.contains("PipeWire: Deinitializing streaming") || line.contains("Encoding complete") {
                    self.clients.clear();
                }
            }
            Kind::Wolf => {}
        }
    }
}

/// The apps installed here.
pub fn find() -> Vec<App> {
    let app = |kind, path: PathBuf| App { kind, path, offset: 0, clients: Vec::new() };
    let mut apps = Vec::new();
    #[cfg(target_os = "linux")]
    {
        // Wolf's documented socket; it runs in Docker, so the socket is there whether or not it runs.
        let wolf = PathBuf::from("/var/run/wolf/wolf.sock");
        if wolf.exists() {
            apps.push(app(Kind::Wolf, wolf));
        }
        let home = crate::home();
        for steam in [".local/share/Steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"] {
            let log = home.join(steam).join("logs/streaming_log.txt");
            if log.is_file() {
                apps.push(app(Kind::Steam, log));
                break;
            }
        }
    }
    #[cfg(windows)]
    {
        // Parsec hosts only on Windows (and macOS). A shared install logs under ProgramData, a
        // per-user one under that user's AppData; the daemon runs as SYSTEM, so it looks in each.
        let shared = std::env::var_os("ProgramData").map(|d| PathBuf::from(d).join(r"Parsec\log.txt"));
        let users = fs::read_dir(r"C:\Users").into_iter().flatten().flatten().map(|u| u.path().join(r"AppData\Roaming\Parsec\log.txt"));
        // The one written last, when several users have it.
        let newest = shared.into_iter().chain(users).filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p))).max();
        if let Some((_, log)) = newest {
            apps.push(app(Kind::Parsec, log));
        }
        let steam = std::env::var_os("ProgramFiles(x86)").map(|d| PathBuf::from(d).join(r"Steam\logs\streaming_log.txt"));
        if let Some(log) = steam.filter(|p| p.is_file()) {
            apps.push(app(Kind::Steam, log));
        }
    }
    apps
}

/// How many streams Wolf runs, from `GET /api/v1/sessions` on its socket.
#[cfg(unix)]
pub fn wolf_sessions(socket: &std::path::Path) -> anyhow::Result<usize> {
    use std::{io::Write, time::Duration};
    let mut s = std::os::unix::net::UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET /api/v1/sessions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut reply = Vec::new();
    // Wolf may keep the connection open; the body ends where Content-Length says.
    let mut buf = [0u8; 8192];
    loop {
        let n = match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if !reply.is_empty() && matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e.into()),
        };
        reply.extend_from_slice(&buf[..n]);
        if let Some(body) = body(&reply) {
            return parse_sessions(body);
        }
    }
    let at = reply.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("Wolf sent no reply"))?;
    parse_sessions(&reply[at + 4..])
}

#[cfg(not(unix))]
pub fn wolf_sessions(_: &std::path::Path) -> anyhow::Result<usize> {
    anyhow::bail!("Wolf runs on Linux only")
}

/// The body once all of it arrived, by Content-Length.
#[cfg_attr(not(unix), allow(dead_code))]
fn body(reply: &[u8]) -> Option<&[u8]> {
    let at = reply.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&reply[..at]).to_ascii_lowercase();
    let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:"))?.trim().parse().ok()?;
    reply.get(at + 4..at + 4 + len)
}

#[cfg_attr(not(unix), allow(dead_code))]
fn parse_sessions(body: &[u8]) -> anyhow::Result<usize> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    Ok(v.get("sessions").and_then(|s| s.as_array()).map(Vec::len).unwrap_or(0))
}

/// Whether a process by this name runs.
fn running(name: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        fs::read_dir("/proc")
            .map(|d| d.flatten().any(|p| fs::read_to_string(p.path().join("comm")).is_ok_and(|c| c.trim_end() == name)))
            .unwrap_or(true)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let image = format!("{name}.exe");
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH", "/FO", "CSV"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            // When it cannot tell, the stream goes on.
            .map_or(true, |o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains(&image.to_ascii_lowercase()))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = name;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn feed(kind: Kind, lines: &[&str]) -> bool {
        let mut a = App { kind, path: PathBuf::new(), offset: 0, clients: Vec::new() };
        for l in lines {
            a.line(l);
        }
        !a.clients.is_empty()
    }

    #[test]
    fn parsec_counts_clients_and_restarts() {
        let on = "[I 2024-03-08 11:12:10] withlqs#9877093 connected.";
        let off = "[I 2024-03-08 11:12:13] withlqs#9877093 disconnected.";
        let other = "[I 2024-03-08 11:12:11] friend#1 connected.";
        assert!(feed(Kind::Parsec, &[on]));
        assert!(!feed(Kind::Parsec, &[on, off]));
        assert!(feed(Kind::Parsec, &[on, other, off]), "one client left");
        assert!(!feed(Kind::Parsec, &[on, "[I 2024-03-08 12:00:00] ===== Parsec: Started ====="]));
        assert!(!feed(Kind::Parsec, &["[D 2024-03-08 11:12:10] Virtual Display Driver State: OK"]));
    }

    #[test]
    fn steam_starts_and_ends() {
        let on = "[2026-10-15 12:00:00][1.0] Streaming started to Samsung Steam TV 1.1.0 at 192.168.1.6:1, audio channels = 2, MTU = 1468";
        assert!(feed(Kind::Steam, &[on]));
        assert!(!feed(Kind::Steam, &[on, "[2026-10-15 12:10:00][601.0] PipeWire: Deinitializing streaming"]));
        assert!(!feed(Kind::Steam, &["[2026-09-14 22:37:19][9711.39] Game Recording - game stopped [gameid=2406770]"]));
    }

    #[test]
    fn logs_are_followed_across_polls_and_restarts() {
        let dir = std::env::temp_dir().join(format!("plugover-follow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log.txt");
        fs::write(&log, "[I 2024-03-08 11:12:10] a#1 conn").unwrap();
        let mut a = App { kind: Kind::Parsec, path: log.clone(), offset: 0, clients: Vec::new() };
        a.read().unwrap();
        assert!(a.clients.is_empty(), "half a line waits");
        fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"ected.\n").unwrap();
        a.read().unwrap();
        assert_eq!(a.clients, ["a#1"]);
        fs::write(&log, "x\n").unwrap();
        a.read().unwrap();
        assert!(a.clients.is_empty(), "a new, shorter log starts over");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wolf_is_asked_on_its_socket() {
        let sock = std::env::temp_dir().join(format!("plugover-wolf-{}.sock", std::process::id()));
        let _ = fs::remove_file(&sock);
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut req = [0u8; 512];
            let n = c.read(&mut req).unwrap();
            assert!(req[..n].starts_with(b"GET /api/v1/sessions "));
            let json = r#"{"success":true,"sessions":[{"app_id":"1"},{"app_id":"2"}]}"#;
            // Wolf keeps the connection open after the reply.
            c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{json}", json.len()).as_bytes()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
        });
        assert_eq!(wolf_sessions(&sock).unwrap(), 2);
        let _ = fs::remove_file(&sock);
    }

    #[test]
    fn wolf_replies_are_read_by_length() {
        let json = r#"{"success":true,"sessions":[{"app_id":"1"}]}"#;
        let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}", json.len()).into_bytes();
        assert_eq!(parse_sessions(body(&reply).unwrap()).unwrap(), 1);
        assert!(body(&reply[..reply.len() - 1]).is_none(), "not all of it yet");
        assert_eq!(parse_sessions(br#"{"success":true,"sessions":[]}"#).unwrap(), 0);
    }
}
