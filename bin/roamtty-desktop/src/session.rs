//! One controller per desktop. The current `serve` writes its pid; a newcomer with the right
//! resume token retires it quietly (SIGUSR2), anyone else kicks it (SIGUSR1) and gets a new token.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
    pub session: String,
    pub resume_token: String,
}

/// 0 = keep going, 1 = kicked by another device, 2 = replaced by the same device.
pub static RETIRED: AtomicU8 = AtomicU8::new(0);

extern "C" fn on_kick(_: libc::c_int) { RETIRED.store(1, Ordering::SeqCst); }
extern "C" fn on_replace(_: libc::c_int) { RETIRED.store(2, Ordering::SeqCst); }

pub fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b64(&b)
}

pub fn b64(b: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() { s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char) } else { s.push('=') }
        }
    }
    s
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn is_serve(pid: i32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).map(|c| {
        let args: Vec<&[u8]> = c.split(|b| *b == 0).collect();
        args.iter().any(|a| a.ends_with(b"roamtty-desktop")) && args.iter().any(|a| *a == b"serve")
    }).unwrap_or(false)
}

/// Takes over the desktop; returns the session the hello should carry.
pub fn claim(resume: Option<&str>, token: Option<&str>) -> Session {
    unsafe {
        libc::signal(libc::SIGUSR1, on_kick as *const () as usize);
        libc::signal(libc::SIGUSR2, on_replace as *const () as usize);
    }
    let dir = paths::desktop_dir();
    let state = dir.join("session.json");
    let stored: Option<Session> = std::fs::read(&state).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let same = matches!((&stored, resume, token), (Some(s), Some(r), Some(t)) if s.session == r && s.resume_token == t);
    let session = match stored {
        Some(s) if same => s,
        Some(s) => Session { session: s.session, resume_token: random_b64(16) },
        None => {
            let mut b = [0u8; 8];
            if let Ok(mut f) = std::fs::File::open("/dev/urandom") { use std::io::Read; let _ = f.read_exact(&mut b); }
            Session { session: hex(&b), resume_token: random_b64(16) }
        }
    };
    let _ = paths::write_private(&state, &serde_json::to_vec(&session).unwrap());
    let pidfile = dir.join("controller.pid");
    if let Some(old) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
        if old != std::process::id() as i32 && is_serve(old) {
            crate::log!("controller {old} {}", if same { "replaced" } else { "kicked" });
            unsafe { libc::kill(old, if same { libc::SIGUSR2 } else { libc::SIGUSR1 }); }
        }
    }
    let _ = paths::write_private(&pidfile, std::process::id().to_string().as_bytes());
    session
}

pub fn release() {
    let pidfile = paths::desktop_dir().join("controller.pid");
    if std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(std::process::id()) {
        let _ = std::fs::remove_file(pidfile);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        assert_eq!(super::b64(b"Man"), "TWFu");
        assert_eq!(super::b64(b"Ma"), "TWE=");
        assert_eq!(super::b64(&[0u8; 16]).len(), 24);
    }
}
