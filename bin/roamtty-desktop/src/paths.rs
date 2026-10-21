//! Everything lives under ~/.roamtty: the binary in bin/, state, logs and libraries in desktop/.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn root() -> PathBuf { home().join(".roamtty") }
pub fn desktop_dir() -> PathBuf { root().join("desktop") }
pub fn lib_dir() -> PathBuf { desktop_dir().join("lib") }
pub fn log_dir() -> PathBuf { desktop_dir().join("log") }
pub fn bin_path() -> PathBuf { root().join("bin").join("roamtty-desktop") }

/// Creates a private directory (0700) and its parents.
pub fn ensure(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Writes a file only this user can read, replacing it atomically.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(p) = path.parent() { ensure(p)?; }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}
