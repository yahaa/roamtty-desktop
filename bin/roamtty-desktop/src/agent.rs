//! macOS: the long-running agent and the `serve` bridge to it (plan §6.4).
//!
//! `Roamtty Desktop.app` runs `roamtty-desktop agent` as a LaunchAgent in the user's GUI session,
//! the only place macOS lets Screen Recording and Accessibility be granted. It listens on
//! ~/.roamtty/desktop/agent.sock (0600) and accepts a connection only with the token it wrote to
//! ~/.roamtty/desktop/token (0600). `serve`, started over SSH, connects, sends the token and its
//! arguments, then copies bytes both ways without reading them.

use crate::paths;
use crate::serve;
use crate::session::{self, Session};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

pub const LABEL: &str = "com.roamtty.desktop";

fn socket() -> PathBuf { paths::desktop_dir().join("agent.sock") }
fn token_path() -> PathBuf { paths::desktop_dir().join("token") }
pub fn app_dir() -> PathBuf { paths::root().join("Roamtty Desktop.app") }
fn plist_path() -> PathBuf { paths::home().join("Library/LaunchAgents").join(format!("{LABEL}.plist")) }

/// The connection being served, with the flag that retires it: 1 kicked, 2 replaced.
static CURRENT: Mutex<Option<Arc<AtomicU8>>> = Mutex::new(None);
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// When the system prompts were last shown: they appear only while someone is connecting.
static ASKED: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Shows the Screen Recording / Accessibility prompts on the Mac, at most once a minute, and only
/// because a phone is trying to connect right now.
fn ask_permissions() {
    let mut asked = ASKED.lock().unwrap();
    if asked.is_some_and(|t| t.elapsed().as_secs() < 60) { return; }
    *asked = Some(std::time::Instant::now());
    crate::mac::request_permissions();
}

pub fn run() -> i32 {
    let _ = paths::ensure(&paths::desktop_dir());
    let token = session::random_b64(24);
    if paths::write_private(&token_path(), token.as_bytes()).is_err() { return 1; }
    let _ = std::fs::remove_file(socket());
    let old = unsafe { libc::umask(0o077) };
    let listener = match UnixListener::bind(socket()) { Ok(l) => l, Err(e) => { crate::log!("agent bind: {e}"); return 1; } };
    unsafe { libc::umask(old); }
    crate::log!("agent up, needs {:?}", crate::mac::needs());
    // macOS applies a newly granted Screen Recording permission only to a fresh process: in the
    // two minutes after a phone asked, while something is still missing and nobody is connected,
    // exit now and then so launchd starts us again.
    let idle_since = Arc::new(Mutex::new(std::time::Instant::now()));
    {
        let idle = idle_since.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let busy = CURRENT.lock().unwrap().is_some();
            let asked_recently = ASKED.lock().unwrap().is_some_and(|t| t.elapsed().as_secs() < 120);
            if asked_recently && !busy && !crate::mac::needs().is_empty() && idle.lock().unwrap().elapsed().as_secs() >= 3 {
                crate::log!("agent restarting to pick up permissions");
                std::process::exit(0);
            }
        });
    }
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let token = token.clone();
        let idle = idle_since.clone();
        std::thread::spawn(move || { handle(conn, &token); *idle.lock().unwrap() = std::time::Instant::now(); });
    }
    0
}

fn handle(conn: UnixStream, token: &str) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() { return; }
    let mut parts = first.trim_end().splitn(2, ' ');
    if parts.next() != Some(token) { crate::log!("agent: bad token"); return; }
    let rest = parts.next().unwrap_or("");
    let mut out = conn;
    if rest == "doctor" {
        let j = serde_json::json!({ "needs": crate::mac::needs(), "agent": true });
        let _ = writeln!(out, "{j}");
        return;
    }
    let Ok(a) = serde_json::from_str::<serde_json::Value>(rest) else { return };
    if !crate::mac::needs().is_empty() { ask_permissions(); }
    let retired = Arc::new(AtomicU8::new(0));
    let claimed = claim(a["resume"].as_str(), a["token"].as_str(), retired.clone());
    let args = serve::Args {
        proto: a["proto"].as_u64().unwrap_or(1) as u32,
        client_long: a["client_long"].as_u64().unwrap_or(1920) as u32,
        resume: None, token: None, retired: retired.clone(), claimed: Some(claimed),
    };
    serve::run_on(args, Box::new(reader), Box::new(out));
    let mut cur = CURRENT.lock().unwrap();
    if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, &retired)) { *cur = None; }
}

/// One controller per desktop: the right resume token replaces the old connection quietly,
/// anything else kicks it and gets a new token.
fn claim(resume: Option<&str>, token: Option<&str>, me: Arc<AtomicU8>) -> Session {
    let mut s = SESSION.lock().unwrap();
    let same = matches!((&*s, resume, token), (Some(x), Some(r), Some(t)) if x.session == r && x.resume_token == t);
    let next = match &*s {
        Some(x) if same => x.clone(),
        Some(x) => Session { session: x.session.clone(), resume_token: session::random_b64(16) },
        None => Session { session: session::random_b64(8).replace(['+', '/', '='], ""), resume_token: session::random_b64(16) },
    };
    *s = Some(next.clone());
    let mut cur = CURRENT.lock().unwrap();
    if let Some(old) = cur.replace(me) { old.store(if same { 2 } else { 1 }, Ordering::SeqCst); }
    next
}

/// `serve` on macOS: hand the SSH channel to the agent.
pub fn bridge(a: &serve::Args) -> i32 {
    let fail = |code: &str, detail: &str| {
        println!("{}", serde_json::json!({ "error": code, "detail": detail }));
        1
    };
    let Ok(token) = std::fs::read_to_string(token_path()) else {
        return fail("not_logged_in", "Roamtty Desktop is not running: log in to the Mac's desktop");
    };
    let Ok(mut conn) = UnixStream::connect(socket()) else {
        return fail("not_logged_in", "Roamtty Desktop is not running: log in to the Mac's desktop");
    };
    let args = serde_json::json!({ "proto": a.proto, "client_long": a.client_long, "resume": a.resume, "token": a.token });
    if writeln!(conn, "{} {}", token.trim(), args).is_err() { return fail("internal", "agent closed"); }
    let mut back = conn.try_clone().unwrap();
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut conn);
        let _ = conn.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = std::io::stdout();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        match back.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => { if stdout.write_all(&buf[..n]).and_then(|_| stdout.flush()).is_err() { break; } }
        }
    }
    0
}

/// The agent's answer to `doctor`, or None when it is not running.
pub fn ask_doctor() -> Option<serde_json::Value> {
    let token = std::fs::read_to_string(token_path()).ok()?;
    let mut conn = UnixStream::connect(socket()).ok()?;
    writeln!(conn, "{} doctor", token.trim()).ok()?;
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|s| s.success())
}

/// Builds `Roamtty Desktop.app` around this binary, signs it (Developer ID builds keep their
/// signature; a bare binary is signed ad hoc), and (re)loads the LaunchAgent in the GUI session.
pub fn install() -> i32 {
    let app = app_dir();
    let macos = app.join("Contents/MacOS");
    if paths::ensure(&macos).is_err() { return 1; }
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).unwrap();
    let target = macos.join("roamtty-desktop");
    let release = app.join("Contents/_CodeSignature").exists() && std::process::Command::new("codesign")
        .args(["--verify", "--strict"]).arg(&app).status().is_ok_and(|s| s.success());
    if exe != target && !release {
        let tmp = target.with_extension("new");
        if std::fs::copy(&exe, &tmp).is_err() || std::fs::rename(&tmp, &target).is_err() { println!("copy failed"); return 1; }
    }
    // A notarized Developer ID bundle (the release) is used as it is: touching it would break the
    // signature. A bare binary gets a bundle and an ad-hoc signature, so TCC can still record it.
    let signed = std::process::Command::new("codesign").args(["-dv", "--verbose=2"]).arg(&app).output()
        .map(|o| String::from_utf8_lossy(&o.stderr).contains("Authority=Developer ID Application")).unwrap_or(false);
    if !signed && std::fs::write(app.join("Contents/Info.plist"), info_plist()).is_err() { return 1; }
    if !signed && !run_quiet("codesign", &["--force", "--deep", "--options", "runtime", "-s", "-", app.to_str().unwrap()]) {
        println!("codesign failed");
        return 1;
    }
    let log = paths::log_dir().join("agent.log");
    let _ = paths::ensure(&paths::log_dir());
    let plist = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>agent</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#, exe = target.display(), log = log.display());
    if let Some(p) = plist_path().parent() { let _ = std::fs::create_dir_all(p); }
    if std::fs::write(plist_path(), plist).is_err() { return 1; }
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let _ = run_quiet("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
    if !run_quiet("launchctl", &["bootstrap", &domain, plist_path().to_str().unwrap()]) {
        println!("launchctl bootstrap failed: nobody logged in to the desktop?");
        return 5;
    }
    for _ in 0..50 {
        if socket().exists() && ask_doctor().is_some() { println!("agent ok"); return 0; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!("agent did not start");
    1
}

pub fn info_plist() -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>{LABEL}</string>
  <key>CFBundleName</key><string>Roamtty Desktop</string>
  <key>CFBundleExecutable</key><string>roamtty-desktop</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{ver}</string>
  <key>CFBundleVersion</key><string>{ver}</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSScreenCaptureUsageDescription</key><string>Roamtty shows this Mac's screen on your phone.</string>
</dict></plist>
"#, ver = env!("CARGO_PKG_VERSION"))
}

pub fn uninstall() {
    let uid = unsafe { libc::getuid() };
    let _ = run_quiet("launchctl", &["bootout", &format!("gui/{uid}/{LABEL}")]);
    let _ = std::fs::remove_file(plist_path());
    let _ = std::fs::remove_dir_all(app_dir());
}
