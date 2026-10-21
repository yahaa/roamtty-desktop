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

## UDP transport

The hello's `udp` object offers a port (60000–61000, the mosh range) and a fresh 128-bit key; the
key travels only over SSH and is never stored. The client sends to the address it reached over SSH.
While UDP is alive (a valid datagram within the last 3 s) the host sends everything over it, and
falls back to the SSH channel otherwise; the client keeps sending heartbeats twice a second, so the
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
- Reliable (`flags` bit0): resent every max(2 × RTT, 80 ms) until a RACK arrives. The client sends keys,
  buttons, text, CONFIG, REQ, ACTION and CLIP_SET reliably; pointer moves, wheel, ACK and PING are not
  (the next one supersedes them). The host sends STATE and CURSOR_SHAPE reliably, with full parity.
- Congestion control (a simplified GCC): the host compares the reported arrival spread with its send
  spread, keeps a trend of the accumulated delay over the last 20 messages, and on overuse sets the rate
  to 85% of what arrived; with over 10% loss it lowers the rate by half the loss rate per second;
  otherwise it raises the rate by 8% a second. The encoder hears of a new rate at most once a second, and
  only for a change of 10% or more. Datagrams are paced at 1.5× the rate, in bursts of up to 8.

`CONFIG` adds `udp` (false turns UDP off) and `udp_client_port`: the host then sends three heartbeats
to that port at the address in `SSH_CLIENT`, so a stateful host firewall lets the client's datagrams in.
`debug_loss` (a 0–1 share of datagrams dropped) works only when the plugin runs with
`ROAMTTY_DESKTOP_TEST=1`, for test fixtures.

`testdata/udp-sealed.bin` (key 00..0f, seq 7, a heartbeat) and `testdata/udp-frags.bin` (a 3000-byte
video message at 50% parity, each fragment as `u16 len | packet`) pin both sides to this layout.
