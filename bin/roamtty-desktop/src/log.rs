//! desktop.log: events and statistics only, never pixels, clipboard or typed text.
//! Rotates at 5 MB and keeps three files.

use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<()> = Mutex::new(());
const MAX: u64 = 5 * 1024 * 1024;

pub fn line(msg: &str) {
    let _g = LOG.lock();
    let dir = crate::paths::log_dir();
    if crate::paths::ensure(&dir).is_err() { return; }
    let path = dir.join("desktop.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX).unwrap_or(false) {
        let _ = std::fs::rename(dir.join("desktop.log.1"), dir.join("desktop.log.2"));
        let _ = std::fs::rename(&path, dir.join("desktop.log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{t} [{}] {msg}", std::process::id());
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::line(&format!($($t)*)) };
}
