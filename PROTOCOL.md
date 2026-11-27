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
 "enc":["h264"],"hw":"openh264","needs":[],"udp":{"port":60123,"key":"base64(16 bytes)"},"audio":["opus"]}
```

`audio` lists the sound codecs the host can send; it is absent on plugins before 0.9.0.

`udp` is absent until the UDP transport ships. `needs` lists what the user must do on the host:
`screen-recording`, `accessibility` (macOS), `not-logged-in`, `locked`, `portal-approval` (Wayland).

Error: `{"error":"<code>","detail":"…"}`, codes `proto_unsupported` (with `min`/`max`), `no_desktop`,
`not_logged_in`, `permission`, `capture_failed`, `encoder_failed`, `internal`. The plugin then exits with 1.

## Frames

After the hello line: `u8 type | u24 len | payload`, all integers big endian, `len` ≤ 16 MiB − 1.
Low-rate control messages carry UTF-8 JSON; high-rate ones fixed binary. Unknown types are ignored.

| type | dir | name | payload |
|---|---|---|---|
| `0x01` | C→S | CONFIG | JSON, changed fields only: `display`, `fps`, `quality` (`auto`/`smooth`/`clear`/`original`), `max_bitrate`, `udp`, `udp_client_port`, `lock_on_close`, `audio` |
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
| `0x84` | S→C | STATE | JSON event: `displays`, `needs`, `locked`, `clip`, `kicked`, `error`, `timing`, `audio` |
| `0x85` | S→C | AUDIO | `u32 seq` `u64 capture_us` + one Opus packet |

`timing` comes about once a second while frames go out: the host's share of a frame's latency,
averaged in µs: `{"frames": n, "capture_us", "convert_us", "encode_us", "encoder"}`. `capture_us` is
grabbing the pixels and finding out whether they changed, counted from when the picture was on the
screen where the host knows that (macOS: the frame's presentation time, so the wait for `serve` to
take it is in it); `convert_us` scaling them and converting them to YUV (0 when the encoder does
that on the GPU), `encode_us` the rest of the encoder's time. VIDEO's `capture_us` is that same
start, so the client's end-to-end figure runs from the screen.

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

## Sound

`CONFIG {"audio":true}` starts sending what plays on the host's default output; `false` stops it,
and so does `REQ pause` until `REQ resume`. Each AUDIO frame is one 20 ms Opus packet of 48 kHz
stereo (about 96 kbps; RFC 7845's pre-skip is 312 samples). `seq` goes up by one per 20 ms, so a gap
is sound that is missing: lost, or not sent because the host was silent (after ten silent packets
nothing goes out until sound returns). `capture_us` is when its last sample was captured, on the
host's clock. Linux takes the default sink's monitor through PulseAudio (or PipeWire's pulse server),
Windows a WASAPI loopback of the default output, macOS ScreenCaptureKit (13 or later).

When the host cannot capture sound, it sends `STATE {"audio":"unavailable","detail":"…"}` once;
the picture carries on. Setting `audio` again tries once more.

## One controller

A newcomer that presents the current `session` and `resume_token` replaces the old connection
silently. Anyone else gets a fresh token and the old connection receives `STATE {"kicked":true}`
before it closes.

## Versioning

Adding optional JSON fields keeps `proto`. Changing a binary layout or meaning bumps it. The
`testdata/` directory holds golden samples of every frame; the apps test their codecs against them
(`UPDATE_GOLDEN=1 cargo test` regenerates them).

## UDP transport

The hello's `udp` object offers a port (60000–61000, the mosh range) and a fresh 128-bit key; the
key travels only over SSH and is never stored. The client sends to the address it reached over SSH.
While UDP is alive (a valid datagram within the last 3 s) the host sends everything over it, and
falls back to the SSH channel otherwise; the client keeps sending heartbeats four times a second, so the
switch back happens on its own. When the SSH channel closes while UDP is alive, the host carries on
until UDP has been silent for 30 s.

**Datagram**: `u8 dir | u64 seq` in the clear (dir 0 client→host, 1 host→client), then AES-128-GCM
of one packet with those 9 bytes as associated data. Nonce: `dir, 0, 0, 0, seq` (12 bytes). Each side
counts `seq` up from 1 and drops sequence numbers already seen in the last 1024. Any datagram that
opens with the key updates the peer address (roaming).

**Packets** (first byte):

| kind | name | fields |
|---|---|---|
| 1 | FRAG | `u32 msg` `u8 flags` (bit0 reliable, bit1 video) `u8 block` `u8 blocks` `u16 index` `u16 k` `u16 m` `u32 len` + shard |
| 2 | RACK | `u16 n` + n × `u32 msg` |
| 3 | HEARTBEAT | `u64 t` (sender µs) `u64 echo` (last `t` heard from the peer, 0 for none) |
| 4 | FEEDBACK | `u16 n` + n × (`u32 msg` `u16 received` `u16 total` `u64 first_us` `u64 last_us` `u8 outcome`: 0 lost, 1 whole, 2 rebuilt with parity) |
| 5 | NACK | `u32 msg` `u8 block` `u16 n` + n × `u16 index` |

**Messages**: every protocol frame (`type | len | payload`) is one message. It is cut into blocks of at
most 160 shards; a block of `len` bytes has `k = ceil(len / 1150)` equal shards of `ceil(len / k)` bytes
(the last zero-padded) and `m` Reed-Solomon parity shards over GF(2^8), polynomial 0x11d, Cauchy matrix
`C[j][i] = 1 / ((k + j) xor i)`. Any `k` shards of a block rebuild it.

- Video: parity 5–30%, twice the measured shard loss plus 5%. The receiver reports each video message
  50 ms after it finished (so late parity counts) and NACKs missing shards when the round trip is under
  40 ms; a message silent for max(300 ms, 3 × RTT) is lost and the client asks for a keyframe (at most
  one a second, and the host sends at most one a second).
- Reliable (`flags` bit0): resent every max(2 × RTT, 80 ms) until a RACK arrives. The client numbers
  its reliable messages 2n + 1 (n = 0, 1, 2, …) and everything else with even ids; the host applies
  reliable messages in n order, holding early ones back, so a key's release never overtakes its press.
  A gap left by a message that never arrives is skipped after 1 s. The client sends keys,
  buttons, text, CONFIG, REQ, ACTION and CLIP_SET reliably; pointer moves, wheel, ACK and PING are not
  (the next one supersedes them). The host sends STATE and CURSOR_SHAPE reliably, with full parity, and AUDIO
  unreliably with full parity (each packet twice): a resend would come too late to play.
- Congestion control (a simplified GCC): the host compares the reported arrival spread with its send
  spread, keeps a trend of the accumulated delay over the last 20 messages, and on overuse sets the rate
  to 85% of what arrived (or of the rate, when under half of it arrived); with over 10% loss it lowers
  the rate by half the loss rate per second; otherwise it raises the rate by 8% a second. The encoder
  hears of a new rate at most once a second, and only for a change of 10% or more. Video datagrams leave
  in bursts of up to 16, then paced at max(90% of the measured bottleneck rate, 2.5 × the rate); the
  bottleneck rate is the 75th percentile, over the last 3 s, of how fast unpaced bursts arrived. A
  message's data shards are sent before its parity is computed. Reliable, AUDIO and control datagrams
  are not paced.

`CONFIG` adds `udp` (false turns UDP off) and `udp_client_port`: the host then sends three heartbeats
to that port at the address in `SSH_CLIENT`, so a stateful host firewall lets the client's datagrams in.
`debug_loss` (a 0–1 share of datagrams dropped) works only when the plugin runs with
`ROAMTTY_DESKTOP_TEST=1`, for test fixtures.

`testdata/udp-sealed.bin` (key 00..0f, seq 7, a heartbeat) and `testdata/udp-frags.bin` (a 3000-byte
video message at 50% parity, each fragment as `u16 len | packet`) pin both sides to this layout.

## Voice mode (`agy /voice` bridge)

Headless subcommand (no display capture, TCC permissions, GUI agent, or OpenH264):

```
ssh exec ~/.roamtty/bin/roamtty-desktop voice --proto 1 [--port 24713] [--resume <session> --token <resume_token>] [--idle-timeout 300]
```

First stdout line (UTF-8 JSON + `\n`):

```json
{"proto":1,"ver":"0.9.9","os":"macos","mode":"voice","mic_port":24713,"session":"9f2c…","resume_token":"base64(16 bytes)","udp":{"port":60123,"key":"base64(16 bytes)"},"codecs":["pcm16"]}
```

- Listens on `127.0.0.1:<mic_port>` (prefers `--port`, defaults to `24713`, evicts any disconnected/idle voice daemon still holding that port on fresh start, and falls back to ephemeral port `0` only if another live process holds it).
- Sending `CONFIG {"shutdown":true}` on explicit hangup terminates the voice daemon immediately and releases `127.0.0.1:<mic_port>` without waiting for `--idle-timeout`.
- Each session runs in a detached per-session daemon (`~/.roamtty/desktop/voice/<session>.sock` on Unix, `<session>.ipc` on Windows, mode 0600) that keeps `127.0.0.1:<mic_port>` bound for `--idle-timeout` seconds (default 300) after both SSH and UDP disconnect so `--resume <session> --token <token>` takes over the exact same `mic_port` and active `agy` streams. Multiple sessions without `--resume` coexist on distinct ports.
- Reuses `u8 type | u24 BE len | payload` framing, `UdpLink` (AES-128-GCM + RS-FEC + roaming) with SSH fallback, and `CONFIG` (`0x01`), `PING` (`0x07`), `PONG` (`0x08`), `STATE` (`0x84`).
- Voice payload integers are **little-endian** (`u32 LE`); PCM is mono **s16le**:

| type | dir | name | UDP mode | payload |
|---|---|---|---|---|
| `0x10` | C→S | `MIC_AUDIO` | Unreliable (FEC 1.0 rec., legacy) | `u32 LE stream_id` + raw 16 kHz mono s16le PCM (even byte count) |
| `0x11` | C→S | `SPK_ACK` | Reliable | `u32 LE stream_id` `u8 ok` (`1` OK, `0` ERR) + UTF-8 error (≤ 235 B, empty if OK) |
| `0x12` | C→S | `MIC_OPUS` | Unreliable (FEC 0.0, in-packet RED) | `u32 LE stream_id` `u32 LE seq` `u32 LE ts_samples` `u8 flags` `u8 red_count` + `red_count × (u8 delta_seq, u16 LE len, opus_bytes)` + primary 20 ms Opus VOIP payload |
| `0x13` | C→S | `VOICE_STATS` | Unreliable (FEC 0.0, every 250 ms) | `u32 LE stream_id` `u16 LE loss_permille` `u16 LE jitter_ms` `u16 LE buffer_ms` `u8 max_burst_loss` `u8 flags` |
| `0x90` | S→C | `MIC_START` | Reliable (FEC 1.0) | `u32 LE stream_id` `u32 LE sample_rate` (`16000`) `u8 channels` (`1`) |
| `0x91` | S→C | `MIC_STOP` | Reliable (FEC 1.0) | `u32 LE stream_id` |
| `0x92` | S→C | `SPK_START` | Reliable (FEC 1.0) | `u32 LE stream_id` `u32 LE sample_rate` `u8 channels` (`1`) |
| `0x93` | S→C | `SPK_AUDIO` | Unreliable (FEC 1.0, legacy) | `u32 LE stream_id` + raw mono s16le PCM at negotiated `sample_rate` (chunked ≤ 960 B) |
| `0x94` | S→C | `SPK_CLEAR` | Reliable (FEC 1.0) | `u32 LE stream_id` |
| `0x95` | S→C | `SPK_STOP` | Reliable (FEC 1.0) | `u32 LE stream_id` |
| `0x96` | S→C | `SPK_OPUS` | Unreliable (FEC 0.0, unpaced, in-packet RED) | Same binary layout as `MIC_OPUS` (`0x12`) |
| `0x97` | S→C | `VOICE_STATS` | Unreliable (FEC 0.0, every 250 ms) | Same 12-byte layout as `VOICE_STATS` (`0x13`), reporting uplink Mic stats to client |

- Codec negotiation keeps `proto: 1` backward-compatible: `VoiceHello` advertises `"codecs": ["opus", "pcm16"]`; clients supporting Opus send `CONFIG {"voice_codec": "opus"}` after hello. Legacy clients stay on `pcm16` (`0x10`/`0x93`).
- `flags` in `MIC_OPUS`/`SPK_OPUS`: bit 0 `DTX` (silence keepalive; primary may be 0 bytes), bit 1 `FEC` (Opus in-band LBRR enabled), bit 2 `SPEECH`, bit 3 `RESET` (first frame after `SPK_START`/`SPK_CLEAR`), upper 4 bits `turn_gen` (wrapping 0..15 incremented on `SPK_CLEAR` to drop reordered pre-barge-in packets).
