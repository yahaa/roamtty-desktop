# roamtty-desktop protocol, version 1

The app starts the plugin over SSH and talks to it on the channel's stdin/stdout.

```
ssh exec ~/.roamtty/bin/roamtty-desktop serve --proto 1 --client 2400x1080@420 [--resume <session> --token <resume_token>]
```

- Exit code `127` (shell: command not found) means the plugin is not installed.
- The first stdout line is UTF-8 JSON followed by `\n`: a hello, or an error.

Hello:

```json
{"proto":1,"ver":"0.1.0","os":"linux","session":"9f2c…","resume_token":"base64(16 bytes)",
 "displays":[{"id":0,"w":1920,"h":1080,"scale":1.0,"x":0,"y":0,"primary":true,"name":"screen"}],
 "enc":["h264"],"hw":"openh264","needs":[],"udp":{"port":60123,"key":"base64(16 bytes)"}}
```

`udp` is absent until the UDP transport ships. `needs` lists what the user must do on the host:
`screen-recording`, `accessibility` (macOS), `not-logged-in`, `locked`, `portal-approval` (Wayland).

Error: `{"error":"<code>","detail":"…"}`, codes `proto_unsupported` (with `min`/`max`), `no_desktop`,
`not_logged_in`, `permission`, `capture_failed`, `encoder_failed`, `internal`. The plugin then exits with 1.

## Frames

After the hello line: `u8 type | u24 len | payload`, all integers big endian, `len` ≤ 16 MiB − 1.
Low-rate control messages carry UTF-8 JSON; high-rate ones fixed binary. Unknown types are ignored.

| type | dir | name | payload |
|---|---|---|---|
| `0x01` | C→S | CONFIG | JSON, changed fields only: `display`, `fps`, `quality` (`auto`/`smooth`/`clear`/`original`), `max_bitrate`, `udp`, `udp_client_port`, `lock_on_close` |
| `0x02` | C→S | INPUT | `u8 kind` + fields below |
| `0x03` | C→S | ACTION | `u8`: 1 show desktop, 2 all windows |
| `0x04` | C→S | REQ | `u8`: 1 keyframe, 2 pause, 3 resume, 4 view only on, 5 view only off, 6 get remote clipboard |
| `0x05` | C→S | ACK | `u32 frame` `u64 recv_us` `u64 decoded_us` |
| `0x06` | C→S | CLIP_SET | UTF-8 text, ≤ 1 MiB |
| `0x07` | both | PING | `u64 t1` (sender clock, µs) |
| `0x08` | both | PONG | `u64 t1` `u64 t2` `u64 t3` |
| `0x81` | S→C | VIDEO | `u32 frame` `u8 flags` (bit0 keyframe, bit1 refine) `u64 capture_us` `u64 encoded_us` `u16 w` `u16 h` + H.264 Annex-B |
| `0x82` | S→C | CURSOR_POS | `u16 x` `u16 y` `u8 visible` `u64 shape_hash` |
| `0x83` | S→C | CURSOR_SHAPE | `u64 hash` `u16 w` `u16 h` `u16 hot_x` `u16 hot_y` + RGBA (straight alpha); each shape once per connection |
| `0x84` | S→C | STATE | JSON event: `displays`, `needs`, `locked`, `clip`, `kicked`, `error` |

| INPUT kind | fields |
|---|---|
| 1 absolute pointer | `u8 display` `u16 x` `u16 y` (native pixels of that display) |
| 2 relative pointer | `i16 dx` `i16 dy` |
| 3 mouse button | `u8 button` (1 left, 2 right, 3 middle) `u8 down` |
| 4 wheel | `i16 dx` `i16 dy`, 1/120 of a notch; positive `dy` scrolls up |
| 5 key | `u16 hid_usage` (USB HID page 7) `u8 down` |
| 6 text | UTF-8, ≤ 4 KiB |

Modifiers are ordinary HID keys with their own down/up; the app tracks latching.

Coordinates are always native pixels of the selected display, independent of the encode size.
Cursor positions are relative to the selected display.

## Flow

- Video is sent only when the screen changed; 300 ms after the last change one refine frame
  (bit1) re-sends the still screen at a higher bitrate.
- At most two frames are unacknowledged; newer screen content replaces skipped frames.
- The rate controller reads queueing delay from ACK round trips: above 50 ms the bitrate drops
  to ×0.8 (and, for `auto`, one step down the ladder 1080p60 → 1080p30 → 720p30 → 540p15 → 540p5);
  otherwise +5% a second, and one step up after 5 s clean. At most one change a second.
- `REQ pause` stops capture and encode until `REQ resume`, which sends a keyframe.

## One controller

A newcomer that presents the current `session` and `resume_token` replaces the old connection
silently. Anyone else gets a fresh token and the old connection receives `STATE {"kicked":true}`
before it closes.

## Versioning

Adding optional JSON fields keeps `proto`. Changing a binary layout or meaning bumps it. The
`testdata/` directory holds golden samples of every frame; the apps test their codecs against them
(`UPDATE_GOLDEN=1 cargo test` regenerates them).
