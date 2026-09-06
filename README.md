# chiaki-stream

[中文说明](./README.zh-CN.md)

A command-line PlayStation 4/5 Remote Play client written in Rust.

`chiaki-stream` pairs with a console, wakes it from standby, and streams its video (H.264 / H.265) and audio (Opus) out as MPEG-TS **pass-through** — no transcoding — to stdout, a file, or a built-in HTTP live server that multiple players can watch.

## Features

- Pairing (`regist`) with the PIN shown on the console screen
- Wake a console in standby (`wakeup`)
- Import hosts registered in the chiaki-ng GUI (`list`, Windows/macOS)
- Video pass-through: H.264, H.265, H.265 HDR
- Audio pass-through: Opus
- MPEG-TS output to stdout, a file, or the built-in HTTP live server
- Built-in HTTP live server (`/live.ts`): multi-viewer fan-out, new clients start at the next video keyframe (4s timeout, then mid-GOP), lagging clients skip missed chunks instead of being disconnected
- Periodic forced IDR (`--idr-interval`) so players can join and resync at any time
- Wall-clock-based timestamps, loss-compensated PTS, audio/video in the same clock domain
- All logs go to stderr; stdout stays clean for piping

## Requirements

- Rust toolchain (edition 2021) and Cargo
- The [`libchiaki`](https://github.com/nanpuyue/libchiaki) crate (Rust bindings of the chiaki-ng C library). Build/install it first — on Windows it needs a prepared chiaki-ng library set (via `LIBCHIAKI_PREFIX`) and an MSYS2/MinGW toolchain; see that repository for details.

## Usage

```text
chiaki-stream [OPTIONS] <COMMAND>

Commands:
  regist   Pair with the console (PIN is shown on the console screen)
  stream   Connect and output MPEG-TS
  wakeup   Wake a console in standby
  list     List hosts registered in chiaki-ng (Windows/macOS)

Global:
  -l, --log-level <LEVEL>   off|error|warning|info|verbose|debug (default: warning)
```

All logs go to stderr. When streaming to stdout, stdout carries TS data only.

### `regist`

```text
--host <IP>             Console IP
--target <1|2|3|4>      1: PS4 <7.0, 2: PS4 >=7.0 <8.0, 3: PS4 >=8.0, 4: PS5
--pin <PIN>             Pairing PIN shown on the console
--account-id <ID>       PSN Account ID (16 hex chars or base64), same as chiaki-ng asks
--online-id <NAME>      PSN Online ID (legacy PS4-1/PS4-2 only)
--console-pin <PIN>     Parental control PIN (default: 0)
```

On success it prints ready-to-copy `--regist-key` / `--morning` values for `stream` / `wakeup`.

### `stream`

```text
[NICKNAME]              A host registered in chiaki-ng (Windows/macOS), connects directly
--host <IP>             Console IP (manual mode)
--console <ps4|ps5>     Console type (manual mode)
--regist-key <KEY>      From `regist`
--morning <HEX32>       rp_key from `regist`
--account-id <ID>       PSN Account ID (optional)
--login-pin <PIN>       Sent automatically when the console asks
--resolution <RES>      360p|540p|720p|1080p (default: 720p, 1080p for PS4 Pro/PS5)
--fps <FPS>             30|60 (default: 60)
--codec <CODEC>         h264|h265|h265-hdr (default: h264)
--bitrate <KBPS>        Default: 8000 (<=720p), 10000 (1080p)
--output <FILE>         Output file, "-" = stdout (default)
--idr-interval <SEC>    Force an IDR every N seconds (default: 2, 0 = off)
--http [ADDR]           HTTP live server (default: 127.0.0.1:8080); implies ignoring --output
```

### `wakeup` / `list`

```text
wakeup --host <IP> --console <ps4|ps5> --regist-key <KEY>
list
```

## Examples

```bash
# Pair with a PS5 (PIN appears on the console screen)
chiaki-stream regist --host 192.168.1.2 --target 4 --pin 12345678 --account-id <base64>

# Wake the console from standby
chiaki-stream wakeup --host 192.168.1.2 --console ps5 --regist-key deadbeef

# List chiaki-ng hosts, then watch through stdout
chiaki-stream list
chiaki-stream stream "MyPS5" | mpv -

# Push to an RTMP server (video passthrough; FLV carries no Opus, audio is transcoded to AAC)
chiaki-stream stream "MyPS5" | ffmpeg -i - -c:v copy -c:a aac -f flv rtmp://server/live/key

# Built-in HTTP live server: open http://127.0.0.1:8080/live.ts in any player
chiaki-stream stream "MyPS5" --http

# Serve LAN viewers explicitly
chiaki-stream stream "MyPS5" --http 0.0.0.0:8080
```

## HTTP Live Server

- Starts only after the console connection is established; before that the port is not opened.
- `GET /live.ts` responds `200` + `Content-Type: video/mp2t` with HTTP/1.1 chunked transfer — players (mpv/VLC/ffplay) open it as a normal network stream.
- A new client is held until the next video keyframe so its first frame decodes immediately; if no keyframe arrives within 4s it starts mid-GOP instead.
- Clients that fall behind the broadcast ring skip missed chunks (live semantics) rather than being disconnected.
- Any other path returns `404` and the connection is closed.
- The server has no authentication; it binds to `127.0.0.1` by default. Use a firewall or reverse proxy if you bind it to a public interface.

## Security Notes

- Regist keys and PSN account IDs are credentials: do not commit or share them.
- The stream is not encrypted by this tool beyond what Remote Play itself provides; keep it inside a trusted network.

## License

This project is licensed under the AGPL-3.0 license. See [`LICENSE`](./LICENSE) for details.

## AI Disclaimer

Parts of this project were generated or assisted by AI tools. The code may contain defects, security issues, or incomplete logic. You are responsible for reviewing, testing, and validating suitability for your environment before use. The authors and contributors provide this project "as is" and disclaim liability for any direct or indirect damages resulting from its use.
