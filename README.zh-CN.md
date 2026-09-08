# chiaki-stream

[English](./README.md)

一个使用 Rust 编写的命令行 PlayStation 4/5 Remote Play 客户端。

`chiaki-stream` 与主机配对、从待机唤醒主机，并把主机的视频（H.264 / H.265）与音频（Opus）以 **透传**（不转码）方式封装为 MPEG-TS 输出到 stdout、文件，或内置的 HTTP 直播服务，供多个播放器观看。

## 功能

- 与主机配对（`regist`，PIN 显示在主机屏幕上）
- 从待机唤醒主机（`wakeup`）
- 导入 chiaki-ng 图形界面已注册的主机（`list`，仅 Windows/macOS）
- 视频透传：H.264、H.265、H.265 HDR
- 音频透传：Opus
- MPEG-TS 输出到 stdout、文件或内置 HTTP 直播服务
- 内置 HTTP 直播服务（`/live.ts`）：多客户端扇出，新客户端从下一个视频关键帧起播（4s 超时则从 GOP 中间开始），落后的客户端跳过错过的块而不是被断开
- 周期性强制 IDR（`--idr-interval`），播放器随时可切入并重新同步
- 时间戳基于 wall clock，丢帧补偿 PTS，音视频同一时钟域
- 日志全部走 stderr；stdout 保持纯净可管道

## 依赖要求

- Rust 工具链（edition 2021）与 Cargo
- [`libchiaki`](https://github.com/nanpuyue/libchiaki) crate（chiaki-ng C 库的 Rust 绑定）。需先构建安装——Windows 上需要准备好的 chiaki-ng 库（通过 `LIBCHIAKI_PREFIX` 指定）与 MSYS2/MinGW 工具链，详见该仓库。

## 用法

```text
chiaki-stream [OPTIONS] <COMMAND>

Commands:
  regist   与主机配对（PIN 显示在主机屏幕上）
  stream   连接主机并输出 MPEG-TS
  wakeup   从待机唤醒主机
  list     列出 chiaki-ng 已注册的主机（仅 Windows/macOS）

选项（必须放在子命令前；非全局）：
  --log-level <LEVEL>   off|error|warning|info|verbose|debug（默认 warning）
```

日志一律走 stderr。以 stdout 输出时，stdout 只承载 TS 数据。

### `regist`

```text
--host <IP>             主机 IP
--target <1|2|3|4>      1: PS4 <7.0, 2: PS4 >=7.0 <8.0, 3: PS4 >=8.0, 4: PS5
--pin <PIN>             主机屏幕上显示的配对 PIN
--account-id <ID>       PSN Account ID（16 位 hex 或 base64），与 chiaki-ng 要求相同
--online-id <NAME>      PSN Online ID（仅旧版 PS4-1/PS4-2）
--console-pin <PIN>     家长控制 PIN（默认 0）
```

成功后打印可直接复制的 `--regist-key` / `--morning`，用于 `stream` / `wakeup`。

### `stream`

```text
[NICKNAME]              chiaki-ng 已注册的主机（Windows/macOS），直接连接
--host <IP>             主机 IP（手动模式）
--console <ps4|ps5>     主机类型（手动模式）
--regist-key <KEY>      来自 regist
--morning <HEX32>       来自 regist 的 rp_key
--account-id <ID>       PSN Account ID（可选）
--login-pin <PIN>       主机要求登录 PIN 时自动发送
--resolution <RES>      360p|540p|720p|1080p（默认 720p，PS4 Pro/PS5 为 1080p）
--fps <FPS>             30|60（默认 60）
--codec <CODEC>         h264|h265|h265-hdr（默认 h264）
--bitrate <KBPS>        默认：<=720p 为 8000，1080p 为 10000
--output <FILE>         输出文件，"-" 表示 stdout（默认）
--idr-interval <SEC>    每 N 秒强制一次 IDR（默认 2，0 关闭）
--http [ADDR]           HTTP 直播服务（默认 127.0.0.1:8080）；设置后忽略 --output
```

### `wakeup` / `list`

```text
wakeup --host <IP> --console <ps4|ps5> --regist-key <KEY>
list
```

## 示例

```bash
# 与 PS5 配对（PIN 显示在主机屏幕上）
chiaki-stream regist --host 192.168.1.2 --target 4 --pin 12345678 --account-id <base64>

# 从待机唤醒主机
chiaki-stream wakeup --host 192.168.1.2 --console ps5 --regist-key deadbeef

# 列出 chiaki-ng 已注册主机，通过 stdout 用 mpv 观看
chiaki-stream list
chiaki-stream stream "MyPS5" | mpv -

# 推流到 RTMP 服务器（视频直通；FLV 不支持 Opus，音频需转 AAC）
chiaki-stream stream "MyPS5" | ffmpeg -i - -c:v copy -c:a aac -f flv rtmp://server/live/key

# 内置 HTTP 直播：播放器打开 http://127.0.0.1:8080/live.ts
chiaki-stream stream "MyPS5" --http

# 显式服务局域网观众
chiaki-stream stream "MyPS5" --http 0.0.0.0:8080
```

## HTTP 直播服务

- 仅在主机连接建立之后启动；之前不占用端口。
- `GET /live.ts` 返回 `200` + `Content-Type: video/mp2t`，HTTP/1.1 分块传输——mpv/VLC/ffplay 当普通网络流打开即可。
- 新客户端会等待下一个视频关键帧，保证首帧可立即解码；4s 内没有关键帧则从 GOP 中间开始。
- 落后于广播环的客户端跳过错过的块（直播语义），不会被断开。
- 其他路径返回 `404` 并断开连接。
- 服务无鉴权，默认只绑定 `127.0.0.1`；如需绑定公网接口请配合防火墙或反向代理。

## 安全说明

- Regist key 与 PSN Account ID 均为凭据：不要提交或分享。
- 本工具在 Remote Play 协议自身加密之外不提供额外加密；请保持在可信网络内使用。

## 许可证

本项目采用 AGPL-3.0 许可证，详见 [`LICENSE`](./LICENSE)。

## AI 免责声明

本项目部分代码由 AI 生成或辅助生成，可能存在缺陷、安全问题或不完整逻辑。使用者需自行完成审查、测试与适配验证。作者与贡献者按 "as is" 提供本项目，不对任何直接或间接损失承担责任。
