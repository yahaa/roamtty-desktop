//! Everything lives under ~/.roamtty: the binary in bin/, state, logs and libraries in desktop/.
//! The Windows service and its helper use C:\ProgramData\Roamtty instead (ROAMTTY_DESKTOP_ROOT).

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn root() -> PathBuf { std::env::var_os("ROAMTTY_DESKTOP_ROOT").map(PathBuf::from).unwrap_or_else(|| home().join(".roamtty")) }
pub fn desktop_dir() -> PathBuf { root().join("desktop") }
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn lib_dir() -> PathBuf { desktop_dir().join("lib") }
pub fn log_dir() -> PathBuf { desktop_dir().join("log") }
pub fn bin_path() -> PathBuf { root().join("bin").join(if cfg!(windows) { "roamtty-desktop.exe" } else { "roamtty-desktop" }) }

/// Creates a private directory (0700) and its parents.
pub fn ensure(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    { use std::os::unix::fs::DirBuilderExt; std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) }
    // The profile directory's ACL already admits only this user and the system.
    #[cfg(not(unix))]
    { std::fs::create_dir_all(dir) }
}

/// Writes a file only this user can read, replacing it atomically.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(p) = path.parent() { ensure(p)?; }
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    { use std::os::unix::fs::OpenOptionsExt; o.mode(0o600); }
    let mut f = o.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}
