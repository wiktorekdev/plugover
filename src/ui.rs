//! The console page without punktfunk: the host daemon serves it on loopback for `plugover ui`.
//! A one-time code from the daemon's own API, which only this user (an administrator on Windows)
//! can reach, buys a cookie. Every other request needs that cookie, must name this address as its
//! host and may not come from another site, so other pages open in the browser get nothing.

use crate::hostd::{Daemon, Request};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

include!(concat!(env!("OUT_DIR"), "/ui.rs"));

pub const PORT: u16 = 7277;
/// How long the code `plugover ui` puts in the address works.
const CODE_LIFE: Duration = Duration::from_secs(60);
/// What the page may ask the daemon; the same list the punktfunk plugin passes through.
const OPS: &[&str] = &["status", "discover", "pair", "forget", "auto", "label", "attach", "detach", "unencrypted", "bt_scan", "bt_pair", "bt_forget"];
const MAX_HEAD: u64 = 16 << 10;
const MAX_BODY: usize = 64 << 10;

/// Who may use the page: one-time codes waiting to be redeemed, and the cookie they buy.
pub struct Gate {
    secret: String,
    codes: Mutex<Vec<(String, Instant)>>,
}

impl Gate {
    pub fn new() -> Self {
        Self { secret: hex::encode(crate::auth::random::<16>()), codes: Mutex::default() }
    }

    pub fn code(&self) -> String {
        let code = hex::encode(crate::auth::random::<16>());
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|(_, at)| at.elapsed() < CODE_LIFE);
        codes.push((code.clone(), Instant::now()));
        code
    }

    fn redeem(&self, code: &str) -> bool {
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|(_, at)| at.elapsed() < CODE_LIFE);
        let found = codes.iter().position(|(c, _)| c == code);
        found.map(|i| codes.remove(i)).is_some()
    }

    fn admits(&self, req: &Req) -> bool {
        req.header("cookie").is_some_and(|c| c.split(';').any(|kv| kv.trim() == format!("plugover={}", self.secret)))
    }
}

pub fn serve(d: Arc<Daemon>) {
    let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("the page for `plugover ui` cannot listen on 127.0.0.1:{PORT}: {e}");
            return;
        }
    };
    for conn in listener.incoming().flatten() {
        let d = d.clone();
        thread::spawn(move || {
            let _ = answer(&d, conn);
        });
    }
}

struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

fn read(s: &TcpStream) -> io::Result<Req> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut r = BufReader::new(s.take(MAX_HEAD + MAX_BODY as u64));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().ok_or_else(|| bad("no method"))?.to_string(), parts.next().ok_or_else(|| bad("no path"))?);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut headers = Vec::new();
    let mut head = line.len() as u64;
    loop {
        line.clear();
        head += r.read_line(&mut line)? as u64;
        if head > MAX_HEAD {
            return Err(bad("headers too long"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let (n, v) = l.split_once(':').ok_or_else(|| bad("bad header"))?;
        headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let mut req = Req { method, path, query, headers, body: Vec::new() };
    let len: usize = req.header("content-length").map_or(Ok(0), str::parse).map_err(|_| bad("bad length"))?;
    if len > MAX_BODY {
        return Err(bad("body too long"));
    }
    req.body.resize(len, 0);
    r.read_exact(&mut req.body)?;
    Ok(req)
}

struct Reply {
    status: &'static str,
    kind: &'static str,
    extra: String,
    body: Vec<u8>,
}

fn text(status: &'static str, body: &str) -> Reply {
    Reply { status, kind: "text/plain; charset=utf-8", extra: String::new(), body: body.as_bytes().to_vec() }
}

fn answer(d: &Arc<Daemon>, mut s: TcpStream) -> io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let reply = match read(&s) {
        Ok(req) => route(d, &req),
        Err(_) => text("400 Bad Request", "bad request"),
    };
    let head = format!(
        "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\ncache-control: no-store\r\nx-content-type-options: nosniff\r\n\
         x-frame-options: DENY\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n{}\r\n",
        reply.status,
        reply.kind,
        reply.body.len(),
        reply.extra
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&reply.body)
}

fn ours(value: &str, scheme: &str) -> bool {
    [format!("{scheme}127.0.0.1:{PORT}"), format!("{scheme}localhost:{PORT}")].iter().any(|o| o == value)
}

fn route(d: &Arc<Daemon>, req: &Req) -> Reply {
    // Another name for this address (DNS rebinding), or a request another site's page started.
    if !req.header("host").is_some_and(|h| ours(h, ""))
        || req.header("origin").is_some_and(|o| !ours(o, "http://"))
        || req.header("sec-fetch-site").is_some_and(|f| f == "cross-site")
    {
        return text("403 Forbidden", "not from here");
    }
    if req.path == "/login" {
        let code = req.query.split('&').find_map(|kv| kv.strip_prefix("code=")).unwrap_or_default();
        if !d.ui.redeem(code) {
            return text("403 Forbidden", "This link is used up or too old. Run `plugover ui` again.");
        }
        let cookie = format!("set-cookie: plugover={}; HttpOnly; SameSite=Strict; Path=/\r\nlocation: /\r\n", d.ui.secret);
        return Reply { status: "303 See Other", kind: "text/plain", extra: cookie, body: Vec::new() };
    }
    if !d.ui.admits(req) {
        return text("403 Forbidden", "Open this page with `plugover ui`.");
    }
    if let Some(op) = req.path.strip_prefix("/api/") {
        return api(d, op, req);
    }
    if req.method != "GET" {
        return text("405 Method Not Allowed", "only GET here");
    }
    let name = match req.path.trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    match UI.iter().find(|(n, _)| *n == name) {
        Some((n, body)) => Reply { status: "200 OK", kind: kind(n), extra: String::new(), body: body.to_vec() },
        None if UI.is_empty() => text("404 Not Found", "This build of Plugover has no page inside; release builds from scripts/build.sh do."),
        None => text("404 Not Found", "not found"),
    }
}

fn api(d: &Arc<Daemon>, op: &str, req: &Req) -> Reply {
    let value = if !OPS.contains(&op) {
        json!({ "ok": false, "error": "unknown action" })
    } else {
        let mut body: Value = if req.body.is_empty() { json!({}) } else { serde_json::from_slice(&req.body).unwrap_or(Value::Null) };
        match body.as_object_mut() {
            Some(fields) => {
                fields.insert("op".into(), json!(op));
                match serde_json::from_value::<Request>(body) {
                    Ok(r) => d.handle(r).unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") })),
                    Err(e) => json!({ "ok": false, "error": format!("bad request: {e}") }),
                }
            }
            None => json!({ "ok": false, "error": "bad request" }),
        }
    };
    Reply { status: "200 OK", kind: "application/json", extra: String::new(), body: value.to_string().into_bytes() }
}

fn kind(name: &str) -> &'static str {
    match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, headers: &[(&str, &str)]) -> Req {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Req {
            method: "GET".into(),
            path: path.into(),
            query: query.into(),
            headers: headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn only_a_redeemed_code_from_this_address_gets_in() {
        let root = std::env::temp_dir().join(format!("plugover-ui-{}", std::process::id()));
        let d = Daemon::open(root.join("sys"), root.join("host.toml")).unwrap();
        let host = ("host", "127.0.0.1:7277");
        assert_eq!(route(&d, &req("/api/status", &[host])).status, "403 Forbidden", "no cookie");

        let code = d.ui.code();
        let login = format!("/login?code={code}");
        assert_eq!(route(&d, &req(&login, &[("host", "evil.example:7277")])).status, "403 Forbidden", "rebound name");
        let ok = route(&d, &req(&login, &[host]));
        assert_eq!(ok.status, "303 See Other");
        assert_eq!(route(&d, &req(&login, &[host])).status, "403 Forbidden", "a code works once");

        let cookie = ok.extra.lines().next().unwrap().trim_start_matches("set-cookie: ").split(';').next().unwrap().to_string();
        let with = |extra: (&'static str, &'static str)| route(&d, &req("/api/status", &[host, ("cookie", cookie.as_str()), extra]));
        let status = with(("accept", "*/*"));
        assert_eq!(status.status, "200 OK");
        assert!(String::from_utf8(status.body).unwrap().contains(r#""ok":true"#));
        assert_eq!(with(("origin", "https://evil.example")).status, "403 Forbidden", "another site's page");
        assert_eq!(with(("sec-fetch-site", "cross-site")).status, "403 Forbidden");
        let wrong = route(&d, &req("/api/session", &[host, ("cookie", cookie.as_str())]));
        assert!(String::from_utf8(wrong.body).unwrap().contains("unknown action"), "the page cannot fake streams");
        let _ = std::fs::remove_dir_all(root);
    }
}
