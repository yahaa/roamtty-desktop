//! The long-running agent and the `serve` bridge to it (plan §6.4), on macOS and Windows.
//!
//! macOS: `Roamtty Desktop.app` runs `roamtty-desktop agent` as a LaunchAgent in the user's GUI
//! session, the only place Screen Recording and Accessibility can be granted; it listens on
//! ~/.roamtty/desktop/agent.sock (0600). Windows: a scheduled task starts it at logon in the user's
//! interactive session (OpenSSH's processes live in Session 0, with no desktop); it listens on a
//! named pipe only its owner may open. Either way a connection must open with the token the agent
//! wrote to ~/.roamtty/desktop/token (only the user can read it). `serve`, started over SSH,
//! connects, sends the token and its arguments, then copies bytes both ways without reading them.

use crate::paths;
use crate::serve;
use crate::session::{self, Session};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

pub use plat::{install, uninstall};
#[cfg(target_os = "macos")]
pub use plat::info_plist;

fn token_path() -> PathBuf { paths::desktop_dir().join("token") }

/// One connection to the agent, both ways.
pub trait Conn: Read + Write + Send {
    fn split(&self) -> std::io::Result<Box<dyn Conn>>;
}

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
    plat::request_permissions();
}

pub fn run() -> i32 {
    plat::detach_console();
    let _ = paths::ensure(&paths::desktop_dir());
    let token = session::random_b64(24);
    if paths::write_private(&token_path(), token.as_bytes()).is_err() { return 1; }
    let listener = match plat::listen() { Ok(l) => l, Err(e) => { crate::log!("agent listen: {e}"); return 1; } };
    crate::log!("agent up, needs {:?}", plat::needs());
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
            if asked_recently && !busy && !plat::needs().is_empty() && idle.lock().unwrap().elapsed().as_secs() >= 3 {
                crate::log!("agent restarting to pick up permissions");
                std::process::exit(0);
            }
        });
    }
    for conn in listener {
        let token = token.clone();
        let idle = idle_since.clone();
        std::thread::spawn(move || { handle(conn, &token); *idle.lock().unwrap() = std::time::Instant::now(); });
    }
    0
}

fn handle(conn: Box<dyn Conn>, token: &str) {
    let Ok(read_half) = conn.split() else { return };
    let mut reader = BufReader::new(read_half);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() { return; }
    let mut parts = first.trim_end().splitn(2, ' ');
    if parts.next() != Some(token) { crate::log!("agent: bad token"); return; }
    let rest = parts.next().unwrap_or("");
    let mut out = conn;
    if rest == "doctor" {
        let j = serde_json::json!({ "needs": plat::needs(), "agent": true });
        let _ = writeln!(out, "{j}");
        return;
    }
    let Ok(a) = serde_json::from_str::<serde_json::Value>(rest) else { return };
    if !plat::needs().is_empty() { ask_permissions(); }
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
        return fail("not_logged_in", "Roamtty Desktop is not running: log in to the computer's desktop");
    };
    let Ok(mut conn) = plat::connect() else {
        return fail("not_logged_in", "Roamtty Desktop is not running: log in to the computer's desktop");
    };
    let args = serde_json::json!({ "proto": a.proto, "client_long": a.client_long, "resume": a.resume, "token": a.token });
    if writeln!(conn, "{} {}", token.trim(), args).is_err() { return fail("internal", "agent closed"); }
    let Ok(mut back) = conn.split() else { return fail("internal", "agent closed") };
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin(), &mut conn);
        // The SSH channel closed: so does the agent's side, which may carry on over UDP.
        drop(conn);
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
    let mut conn = plat::connect().ok()?;
    writeln!(conn, "{} doctor", token.trim()).ok()?;
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

#[cfg(target_os = "macos")]
mod plat {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub const LABEL: &str = "com.roamtty.desktop";
    fn socket() -> PathBuf { paths::desktop_dir().join("agent.sock") }
    pub fn app_dir() -> PathBuf { paths::root().join("Roamtty Desktop.app") }
    fn plist_path() -> PathBuf { paths::home().join("Library/LaunchAgents").join(format!("{LABEL}.plist")) }

    impl Conn for UnixStream {
        fn split(&self) -> std::io::Result<Box<dyn Conn>> { Ok(Box::new(self.try_clone()?)) }
    }

    pub fn needs() -> Vec<String> { crate::mac::needs() }
    pub fn detach_console() {}
    pub fn request_permissions() { crate::mac::request_permissions() }

    pub fn listen() -> std::io::Result<Box<dyn Iterator<Item = Box<dyn Conn>>>> {
        let _ = std::fs::remove_file(socket());
        let old = unsafe { libc::umask(0o077) };
        let l = UnixListener::bind(socket());
        unsafe { libc::umask(old); }
        let l = l?;
        Ok(Box::new(std::iter::from_fn(move || loop {
            if let Ok((c, _)) = l.accept() { return Some(Box::new(c) as Box<dyn Conn>); }
        })))
    }

    pub fn connect() -> std::io::Result<Box<dyn Conn>> { Ok(Box::new(UnixStream::connect(socket())?)) }

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
        let job = format!("{domain}/{LABEL}");
        let _ = run_quiet("launchctl", &["bootout", &job]);
        // bootout returns before a busy agent (one serving a capture) has exited, and a bootstrap
        // then fails with EIO, leaving no agent at all: wait for the old job to go, then retry.
        for _ in 0..50 {
            if !run_quiet("launchctl", &["print", &job]) { break; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let bootstrap = || run_quiet("launchctl", &["bootstrap", &domain, plist_path().to_str().unwrap()]);
        if !(0..5).any(|i| { if i > 0 { std::thread::sleep(std::time::Duration::from_millis(500)); } bootstrap() }) {
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
}

#[cfg(windows)]
mod plat {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING};
    use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Console::FreeConsole;
    use windows::Win32::System::Threading::CreateEventW;
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT};

    const TASK: &str = "Roamtty Desktop";

    fn pipe_name() -> String {
        let user: String = std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_alphanumeric()).collect();
        format!(r"\\.\pipe\roamtty-desktop-{user}")
    }

    /// A pipe end opened for overlapped I/O, shared by its reading and writing halves. Synchronous
    /// I/O on one file object is serialized: with a read waiting for the phone's next message, the
    /// other half's write (the picture) waited behind it, and serve stalled right after the hello.
    struct Pipe(Arc<OwnedHandle>);

    fn os_err(e: windows::core::Error) -> std::io::Error { std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF) }

    impl Pipe {
        fn new(h: HANDLE) -> Pipe { Pipe(Arc::new(unsafe { OwnedHandle::from_raw_handle(h.0 as RawHandle) })) }

        /// Starts one operation with its own event and waits for it.
        fn io(&self, f: impl FnOnce(HANDLE, *mut OVERLAPPED) -> windows::core::Result<()>) -> std::io::Result<usize> {
            let h = HANDLE(self.0.as_raw_handle() as _);
            let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(os_err)?;
            let event = unsafe { OwnedHandle::from_raw_handle(event.0 as RawHandle) };
            let mut ov = OVERLAPPED { hEvent: HANDLE(event.as_raw_handle() as _), ..Default::default() };
            if let Err(e) = f(h, &mut ov) {
                if e.code() != ERROR_IO_PENDING.to_hresult() { return Err(os_err(e)); }
            }
            let mut n = 0u32;
            unsafe { GetOverlappedResult(h, &ov, &mut n, true) }.map_err(os_err)?;
            Ok(n as usize)
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.io(|h, ov| unsafe { ReadFile(h, Some(buf), None, Some(ov)) }) {
                Err(e) if e.raw_os_error() == Some(ERROR_BROKEN_PIPE.0 as i32) => Ok(0),
                r => r,
            }
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.io(|h, ov| unsafe { WriteFile(h, Some(buf), None, Some(ov)) }) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    impl Conn for Pipe {
        fn split(&self) -> std::io::Result<Box<dyn Conn>> { Ok(Box::new(Pipe(self.0.clone()))) }
    }

    pub fn needs() -> Vec<String> { Vec::new() }
    pub fn request_permissions() {}

    /// The logon task gives the agent a console window on the user's desktop; it lets go of it, and
    /// the window closes (closing it by hand would have ended the agent).
    pub fn detach_console() { unsafe { let _ = FreeConsole(); } }

    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

    /// Accepts pipe clients forever. The pipe's DACL admits only its owner (and SYSTEM).
    pub fn listen() -> std::io::Result<Box<dyn Iterator<Item = Box<dyn Conn>>>> {
        let name = wide(&pipe_name());
        let sddl = wide("D:P(A;;GA;;;OW)(A;;GA;;;SY)");
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // The descriptor lives for the whole process; the attributes are made afresh for each pipe
        // (a pointer to ones on this function's stack dangled once it returned, and no pipe was made).
        let sd_ptr = sd.0 as usize;
        let mut failed = false;
        Ok(Box::new(std::iter::from_fn(move || loop {
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd_ptr as *mut _, bInheritHandle: false.into() };
            let h = unsafe {
                CreateNamedPipeW(PCWSTR(name.as_ptr()), PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES, 1 << 16, 1 << 16, 0, Some(&sa))
            };
            if h.is_invalid() {
                if !failed { crate::log!("agent pipe: {}", std::io::Error::last_os_error()); failed = true; }
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
            let pipe = Pipe::new(h);
            match pipe.io(|h, ov| unsafe { ConnectNamedPipe(h, Some(ov)) }) {
                // ERROR_PIPE_CONNECTED: a client was quicker, a connection too.
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED.0 as i32) => {}
                Err(_) => continue,
            }
            return Some(Box::new(pipe) as Box<dyn Conn>);
        })))
    }

    pub fn connect() -> std::io::Result<Box<dyn Conn>> {
        let name = wide(&pipe_name());
        let h = unsafe {
            CreateFileW(PCWSTR(name.as_ptr()), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAG_OVERLAPPED, None)
        }.map_err(os_err)?;
        Ok(Box::new(Pipe::new(h)))
    }

    fn schtasks(args: &[&str]) -> bool {
        std::process::Command::new("schtasks").args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|s| s.success())
    }

    /// A logon task in the user's interactive session (/IT), started now as well.
    pub fn install() -> i32 {
        let exe = paths::bin_path();
        let tr = format!("\"{}\" agent", exe.display());
        if !schtasks(&["/Create", "/F", "/TN", TASK, "/SC", "ONLOGON", "/RL", "LIMITED", "/IT", "/TR", &tr]) {
            println!("schtasks /Create failed");
            return 5;
        }
        // An inbound rule for the UDP link, so Windows Firewall does not stop the desktop with a prompt
        // the first time serve listens. Needs an elevated (administrator) login; skipped otherwise.
        // Inbound rules for this exe go first, the Block ones Windows makes when its prompt was
        // dismissed among them: a block rule beats any allow rule.
        let prog = format!("program={}", exe.display());
        let _ = netsh(&["delete", "rule", "name=all", "dir=in", &prog]);
        if !netsh(&["add", "rule", "name=Roamtty Desktop", "dir=in", "action=allow", "protocol=UDP", "profile=any", &prog]) {
            println!("firewall rule not added (not an administrator): Windows may ask once");
        }
        let _ = schtasks(&["/Run", "/TN", TASK]);
        for _ in 0..50 {
            if ask_doctor().is_some() { println!("agent ok"); return 0; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        println!("agent did not start: is the user logged in to the desktop?");
        1
    }

    fn netsh(args: &[&str]) -> bool {
        std::process::Command::new("netsh").args(["advfirewall", "firewall"]).args(args).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    }

    pub fn uninstall() {
        let _ = netsh(&["delete", "rule", "name=Roamtty Desktop"]);
        let _ = schtasks(&["/End", "/TN", TASK]);
        let _ = schtasks(&["/Delete", "/F", "/TN", TASK]);
    }
}
