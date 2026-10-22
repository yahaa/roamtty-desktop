//! The Windows service: the desktop before anyone logs in, and on the lock screen.
//!
//! Without it the agent starts at logon in the user's session, so a PC restarted by Windows Update
//! sits at its sign-in screen with nothing to connect to, and a locked one shows the secure desktop,
//! which an ordinary process can neither see nor type into. The service (LocalSystem, automatic
//! start) keeps one helper, `agent --system`, running as SYSTEM in whichever session owns the
//! console; the helper follows the input desktop (sign-in, lock screen, the user's own), so the
//! phone sees and types into all of them.
//!
//! Installing it needs an administrator. Everything it runs or loads lives in
//! C:\ProgramData\Roamtty, which only SYSTEM and administrators may change (the installing user
//! may read it, for the agent's token): a SYSTEM process must never run a file its user could
//! swap. The helper's pipe admits SYSTEM and the user who installed the service, nobody else.

use crate::paths;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, SetTokenInformation, TokenElevation, TokenPrimary, TokenSessionId,
    TokenUser, TOKEN_ALL_ACCESS, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

pub const SERVICE: &str = "RoamttyDesktop";
/// The helper's pipe; the logon-task agent's carries the user's name instead.
pub const PIPE: &str = r"\\.\pipe\roamtty-desktop-system";

/// C:\ProgramData\Roamtty: the service's bin\ and desktop\ (lib, log, token), as ~/.roamtty is the user's.
pub fn root() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("Roamtty")
}

/// The string SID of the user allowed to use the helper, written at install.
pub fn allowed_sid() -> Option<String> {
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(root().join("desktop").join("service.json")).ok()?).ok()?;
    j["sid"].as_str().map(String::from).filter(|s| s.starts_with("S-1-") && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

/// This process's token is elevated (an administrator past UAC, or SYSTEM).
pub fn elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() { return false; }
        let mut e = TOKEN_ELEVATION::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(token, TokenElevation, Some(&mut e as *mut _ as *mut c_void), size_of::<TOKEN_ELEVATION>() as u32, &mut n).is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// The string SID of the user running this process.
fn current_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut buf = vec![0u8; 256];
        let mut n = 0u32;
        let r = GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), buf.len() as u32, &mut n);
        let _ = CloseHandle(token);
        r.ok()?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).ok()?;
        let out = s.to_string().ok();
        windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(s.0 as *mut c_void)));
        out
    }
}

fn run(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().is_ok_and(|s| s.success())
}

pub fn installed() -> bool { run("sc.exe", &["query", SERVICE]) }

/// `sc stop`, then waits (10 s at most) until the service and its helper are gone.
fn stop() {
    let _ = run("sc.exe", &["stop", SERVICE]);
    for _ in 0..100 {
        let out = std::process::Command::new("sc.exe").args(["query", SERVICE]).output();
        if out.map_or(true, |o| String::from_utf8_lossy(&o.stdout).contains("STOPPED") || !o.status.success()) { break; }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Installs (or updates) the service from this binary and the user's OpenH264. Needs an elevated
/// token; the user it runs as becomes the one the helper serves.
pub fn install() -> i32 {
    let Some(sid) = current_sid() else { println!("service: no user SID"); return 5 };
    let root = root();
    let (bin, desktop) = (root.join("bin"), root.join("desktop"));
    for d in [&bin, &desktop.join("lib"), &desktop.join("log")] {
        if std::fs::create_dir_all(d).is_err() { println!("service: cannot create {}", d.display()); return 5; }
    }
    // Only SYSTEM and administrators may change anything here; the user may read (the token).
    let r = root.display().to_string();
    if !run("icacls.exe", &[&r, "/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", &format!("*{sid}:(OI)(CI)RX")]) {
        println!("service: icacls failed");
        return 5;
    }
    stop();
    let exe = bin.join("roamtty-desktop.exe");
    let me = match std::env::current_exe() { Ok(p) => p, Err(e) => { println!("service: {e}"); return 5 } };
    if me != exe && std::fs::copy(&me, &exe).is_err() { println!("service: cannot copy the binary"); return 5; }
    if let Some(name) = crate::openh264enc::openh264_name() {
        let from = paths::lib_dir().join(name);
        if std::fs::copy(&from, desktop.join("lib").join(name)).is_err() { println!("service: no OpenH264 at {}", from.display()); return 5; }
    }
    let config = serde_json::json!({ "sid": sid, "ver": env!("CARGO_PKG_VERSION") }).to_string();
    if std::fs::write(desktop.join("service.json"), config).is_err() { println!("service: cannot write its settings"); return 5; }
    let bin_path = format!("\"{}\" service", exe.display());
    let created = if installed() {
        run("sc.exe", &["config", SERVICE, "binPath=", &bin_path, "start=", "auto"])
    } else {
        run("sc.exe", &["create", SERVICE, "binPath=", &bin_path, "start=", "auto", "DisplayName=", "Roamtty Desktop"])
    };
    if !created { println!("service: sc create failed"); return 5; }
    let _ = run("sc.exe", &["description", SERVICE, "Remote desktop for the Roamtty app: the sign-in screen, the lock screen and the desktop."]);
    // Restarted after a crash: 5 s, 5 s, then a minute.
    let _ = run("sc.exe", &["failure", SERVICE, "reset=", "86400", "actions=", "restart/5000/restart/5000/restart/60000"]);
    let prog = format!("program={}", exe.display());
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", "name=all", "dir=in", &prog]);
    let _ = run("netsh", &["advfirewall", "firewall", "add", "rule", "name=Roamtty Desktop (service)", "dir=in", "action=allow", "protocol=UDP", "profile=any", &prog]);
    if !run("sc.exe", &["start", SERVICE]) { println!("service: sc start failed"); return 5; }
    println!("service ok");
    0
}

pub fn uninstall() {
    stop();
    let _ = run("sc.exe", &["delete", SERVICE]);
    let _ = run("netsh", &["advfirewall", "firewall", "delete", "rule", "name=Roamtty Desktop (service)"]);
    let _ = std::fs::remove_dir_all(root());
}

static STATUS: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);
static STOPPING: AtomicBool = AtomicBool::new(false);

fn report(state: SERVICE_STATUS_CURRENT_STATE) {
    let Some(h) = *STATUS.lock().unwrap() else { return };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS, dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWaitHint: 5000, ..Default::default()
    };
    unsafe { let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(h as *mut c_void), &status); }
}

unsafe extern "system" fn control(code: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    if code == SERVICE_CONTROL_STOP || code == SERVICE_CONTROL_SHUTDOWN { STOPPING.store(true, Ordering::SeqCst); }
    0
}

unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let name = wide(SERVICE);
    let Ok(h) = (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None) }) else { return };
    *STATUS.lock().unwrap() = Some(h.0 as isize);
    report(SERVICE_START_PENDING);
    report(SERVICE_RUNNING);
    supervise();
    report(SERVICE_STOPPED);
}

/// `roamtty-desktop service`, as the service control manager runs it.
pub fn run_service() -> i32 {
    // Everything the helper touches is under the service's root, not a user's profile.
    unsafe { std::env::set_var("ROAMTTY_DESKTOP_ROOT", root()) };
    let mut name = wide(SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_mut_ptr()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    match unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        Ok(()) => 0,
        Err(e) => { eprintln!("not started by the service control manager: {e}"); 1 }
    }
}

struct Helper { process: HANDLE, session: u32 }

impl Helper {
    fn alive(&self) -> bool {
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.process, &mut code).is_ok() && code == 259 } // STILL_ACTIVE
    }
    fn end(self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = WaitForSingleObject(self.process, 3000);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Keeps one helper in the console's session: a new one when the session changes (sign-in,
/// switching user, a Remote Desktop session taking the console) or when it exited.
fn supervise() {
    crate::log!("service up {}", env!("CARGO_PKG_VERSION"));
    let mut helper: Option<Helper> = None;
    let mut failures = 0u32;
    while !STOPPING.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if let Some(h) = helper.take() {
            if h.alive() && h.session == session { helper = Some(h); } else {
                crate::log!("service: helper in session {} {}", h.session, if h.alive() { "replaced" } else { "exited" });
                h.end();
            }
        }
        // 0xFFFFFFFF: between sessions, nobody owns the console for a moment.
        if helper.is_none() && session != u32::MAX {
            match launch(session) {
                Ok(h) => { crate::log!("service: helper started in session {session}"); helper = Some(h); failures = 0; }
                Err(e) => { failures += 1; if failures <= 3 { crate::log!("service: helper failed in session {session}: {e}"); } }
            }
        }
        if let Some(h) = &helper { unsafe { let _ = WaitForSingleObject(h.process, 1000); } }
        else { std::thread::sleep(std::time::Duration::from_secs(1)); }
    }
    if let Some(h) = helper { h.end(); }
    crate::log!("service stopped");
}

/// Starts `agent --system` as SYSTEM in `session`, on its interactive window station.
fn launch(session: u32) -> Result<Helper, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own).map_err(|e| e.to_string())?;
        let mut token = HANDLE::default();
        let r = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
        let _ = CloseHandle(own);
        r.map_err(|e| e.to_string())?;
        let set = SetTokenInformation(token, TokenSessionId, &session as *const u32 as *const c_void, 4);
        if let Err(e) = set { let _ = CloseHandle(token); return Err(e.to_string()); }
        let mut desktop = wide(r"winsta0\default");
        let si = STARTUPINFOW { cb: size_of::<STARTUPINFOW>() as u32, lpDesktop: PWSTR(desktop.as_mut_ptr()), ..Default::default() };
        let mut pi = PROCESS_INFORMATION::default();
        let mut cmd = wide(&format!("\"{}\" agent --system", exe.display()));
        let r = CreateProcessAsUserW(Some(token), None, Some(PWSTR(cmd.as_mut_ptr())), None, None, false, CREATE_NO_WINDOW, None, None, &si, &mut pi);
        let _ = CloseHandle(token);
        r.map_err(|e| e.to_string())?;
        let _ = CloseHandle(pi.hThread);
        Ok(Helper { process: pi.hProcess, session })
    }
}
