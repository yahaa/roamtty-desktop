//! roamtty-desktop: the host side of Roamtty's remote desktop. The app starts `serve` over SSH;
//! the other subcommands install, diagnose and remove it.

mod encode;
mod keymap;
mod log;
mod paths;
mod serve;
mod session;
mod x11;

use std::path::Path;

const OPENH264_URL: &str = "http://ciscobinary.openh264.org";

fn usage() -> i32 {
    eprintln!("usage: roamtty-desktop serve --proto N --client WxH@DPI [--resume S --token T]\n       roamtty-desktop doctor [--json] | install [--openh264 FILE.bz2] | uninstall | --version");
    2
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("roamtty-desktop {} proto {}", env!("CARGO_PKG_VERSION"), proto::PROTO);
            0
        }
        Some("serve") => serve_cmd(&args[1..]),
        Some("doctor") => doctor(),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        _ => usage(),
    };
    std::process::exit(code);
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn serve_cmd(args: &[String]) -> i32 {
    let proto = flag(args, "--proto").and_then(|p| p.parse().ok()).unwrap_or(proto::PROTO);
    // --client 2400x1080@420: the long edge caps the encode size.
    let client_long = flag(args, "--client")
        .and_then(|c| c.split('@').next())
        .and_then(|wh| wh.split_once('x'))
        .and_then(|(w, h)| Some(w.parse::<u32>().ok()?.max(h.parse::<u32>().ok()?)))
        .unwrap_or(1920);
    serve::run(serve::Args { proto, client_long, resume: flag(args, "--resume").map(String::from), token: flag(args, "--token").map(String::from) })
}

fn doctor() -> i32 {
    let display = x11::find_display();
    let lib = encode::openh264_path();
    let encoder = lib.as_ref().filter(|p| p.exists()).map(|_| "openh264");
    let displays = x11::X11::open().map(|x| x.displays()).unwrap_or_default();
    let mut needs: Vec<&str> = Vec::new();
    if display.is_none() { needs.push("not-logged-in"); }
    let last_error = std::fs::read_to_string(paths::log_dir().join("desktop.log")).ok()
        .and_then(|l| l.lines().rev().find(|x| x.contains("failed")).map(String::from));
    let j = serde_json::json!({
        "ver": env!("CARGO_PKG_VERSION"), "proto": proto::PROTO, "os": "linux", "arch": std::env::consts::ARCH,
        "display": display.map(|d| d.0), "encoder": encoder, "displays": displays, "needs": needs, "last_error": last_error,
    });
    println!("{j}");
    0
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// Installs Cisco's OpenH264 from a .bz2 the app uploaded, or downloads it here (10 s at most).
fn install(args: &[String]) -> i32 {
    let Some(name) = encode::openh264_name() else { eprintln!("unsupported architecture"); return 1 };
    let lib = paths::lib_dir().join(name);
    if lib.exists() && openh264::OpenH264API::from_blob_path(&lib).is_ok() {
        println!("openh264 ok");
        return 0;
    }
    if paths::ensure(&paths::lib_dir()).is_err() { return 1; }
    let packed = match flag(args, "--openh264") {
        Some(f) => std::fs::read(f).map_err(|e| e.to_string()),
        None => download(&format!("{OPENH264_URL}/{name}.bz2")),
    };
    let packed = match packed {
        Ok(p) => p,
        Err(e) => { println!("openh264 download failed: {e}"); return 3; }
    };
    let mut raw = Vec::new();
    if let Err(e) = std::io::Read::read_to_end(&mut bzip2_rs::DecoderReader::new(packed.as_slice()), &mut raw) {
        println!("openh264 unpack failed: {e}");
        return 1;
    }
    let tmp = lib.with_extension("part");
    if std::fs::write(&tmp, &raw).is_err() { return 1; }
    // Only Cisco's own builds are loaded: from_blob_path checks the hash.
    if let Err(e) = openh264::OpenH264API::from_blob_path(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        println!("openh264 rejected ({}): {e}", sha256_hex(&raw));
        return 1;
    }
    if std::fs::rename(&tmp, &lib).is_err() { return 1; }
    log!("openh264 installed");
    println!("openh264 ok");
    0
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    for (tool, args) in [("curl", vec!["-fsSL", "--max-time", "10", url]), ("wget", vec!["-q", "-T", "10", "-O", "-", url])] {
        if let Ok(o) = std::process::Command::new(tool).args(&args).output() {
            if o.status.success() && !o.stdout.is_empty() { return Ok(o.stdout); }
        }
    }
    Err("neither curl nor wget could fetch it".into())
}

fn uninstall() -> i32 {
    let _ = std::fs::remove_dir_all(paths::desktop_dir());
    let bin = paths::bin_path();
    if Path::new(&bin).exists() { let _ = std::fs::remove_file(bin); }
    println!("uninstalled");
    0
}
