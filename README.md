# roamtty-desktop

The host side of [Roamtty](https://github.com/yahaa/roamtty)'s remote desktop: a single binary that
the app installs over SSH and starts as `roamtty-desktop serve`. It captures the screen, encodes
H.264 and injects input, all over the SSH channel that started it. It listens on no TCP port.

| Platform | Status |
|---|---|
| Linux X11 | capture (MIT-SHM, woken by DAMAGE), cursor (XFIXES), input and text (XTEST), clipboard, OpenH264 |
| Transport | SSH channel, and UDP (AES-128-GCM, Reed-Solomon FEC, NACK, simplified GCC, pacing, roaming) |
| Linux Wayland (wlroots: sway, Hyprland, labwc, river) | capture (wlr-screencopy, cursor drawn in), input (wlr-virtual-pointer, virtual-keyboard with spliced keymaps for any character), clipboard (wlr-data-control) |
| macOS 13+ | ScreenCaptureKit, VideoToolbox, CGEvent; a notarized LaunchAgent app |
| Windows 10/11 | DXGI, Media Foundation hardware H.264 (else OpenH264), SendInput; a logon task, or the service (below) |
| GNOME / KDE Wayland | planned (see roamtty issue #184) |
| Sound | Opus 48 kHz stereo: PulseAudio/PipeWire monitor (Linux), WASAPI loopback (Windows), ScreenCaptureKit (macOS) |

## Commands

```
roamtty-desktop serve --proto 1 --client 2400x1080@420   # what the app runs
roamtty-desktop install [--openh264 FILE.bz2]            # fetch Cisco OpenH264 (or use an uploaded copy)
roamtty-desktop doctor                                   # JSON diagnostics
roamtty-desktop uninstall                                # an administrator also removes the service
roamtty-desktop encode-test [--cpu]                      # Windows: time the hardware encoder
roamtty-desktop --version
```

Files live in `~/.roamtty/`: the binary in `bin/`, OpenH264 in `desktop/lib/`, logs in
`desktop/log/desktop.log` (events and statistics only, never pixels, clipboard or typed text).

## The Windows service

Run by an administrator (an elevated SSH login, or an administrator PowerShell on the PC),
`install` sets up the `RoamttyDesktop` service instead of the logon task. It keeps
`agent --system` running as SYSTEM in the session that owns the console and follows the input
desktop, so the phone also reaches the sign-in screen after a restart and the lock screen. Its
binary, OpenH264, token and logs are in `C:\ProgramData\Roamtty`, which only SYSTEM and
administrators may change; the pipe admits SYSTEM and the user who installed it. Updating the
service needs an administrator again; without one, `install` updates only the user's copy.

```
& "$env:USERPROFILE\.roamtty\bin\roamtty-desktop.exe" install     # in an administrator PowerShell
```

## H.264 and patents

Software encoding uses Cisco's OpenH264 binary, downloaded from `ciscobinary.openh264.org` at
install time and checked against the known SHA-256 of Cisco's release before it is loaded. Only
Cisco's own binary is covered by Cisco's patent licence, so it is never built or bundled here.
OpenH264 is a shared library, which is why Linux builds link glibc dynamically instead of musl.

## Building

```
cargo test --workspace
cargo build --release
scripts/package.sh 0.1.0 linux-x86_64=target/release/roamtty-desktop
```

Tagging `vX.Y.Z` builds every target in GitHub Actions and attaches the `.gz` files and
`manifest.json` to the release. The app pins one version with `scripts/desktop/pin.sh`.

The wire protocol is in [PROTOCOL.md](PROTOCOL.md). Licensed under Apache-2.0.
