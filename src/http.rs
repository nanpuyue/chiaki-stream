//! HTTP-TS 直播服务: 把 MPEG-TS 输出以 HTTP 长连接扇出给多个播放器。
//!
//! 标准做法: GET /live.ts 返回 200 + `Content-Type: video/mp2t`, 无
//! Content-Length, HTTP/1.1 分块传输 (hyper 自动), VLC/mpv/ffmpeg 当普通
//! 网络流打开; 其他路径一律 404 并断开连接。
//! 新客户端在请求处理器里先等视频关键帧再开始发流 (保证首帧可立即解码),
//! 最多等 4s, 超时直接从 GOP 中间开始; 客户端落后于广播环时跳过错过的块
//! (直播语义)。扇出结构参考 rtmp-forwarder 的 FlvManager/subscribe_flv。

use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use mpeg2ts::ts::TsPacket;

use crate::mux::{VideoCodec, VIDEO_PID};

/// 广播环容量 (块数, 全体客户端共享); 客户端落后超过它就跳过错过的块。
const BROADCAST_CAPACITY: usize = 1024;

/// 每客户端 body 通道的缓冲块数 (广播环 -> HTTP 响应的中转)。
const CLIENT_BUFFER_CHUNKS: usize = 128;

/// 新客户端等待首个视频关键帧的最长时间; 超时直接从 GOP 中间开始发流。
/// (从关键帧起播只是为了让播放器不报错, 不能为此让客户端一直等。)
const KEYFRAME_WAIT: Duration = Duration::from_secs(4);

/// HTTP 服务的共享状态。
#[derive(Clone)]
struct AppState {
    codec: VideoCodec,
    keyframe_wait: Duration,
    tx: broadcast::Sender<Bytes>,
}

/// mux 侧输出: 把每次 flush 的 TS 块投进广播环。
/// 没有订阅者时直接跳过, 永不阻塞 mux。
struct Broadcaster {
    tx: broadcast::Sender<Bytes>,
}

impl Write for Broadcaster {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() && self.tx.receiver_count() > 0 {
            let _ = self.tx.send(Bytes::copy_from_slice(buf));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 等待首个视频关键帧的结果。
enum FirstKeyframe {
    /// 命中关键帧, 从它开始。
    Found(Bytes),
    /// 超时, 从 GOP 中间开始。
    Timeout,
    /// 等待期间流结束。
    Closed,
}

/// 逐块消费广播直到首个视频关键帧; 最多等 `wait`, 超时返回
/// [`FirstKeyframe::Timeout`]。等待期间到达的非关键帧块在这里被丢弃,
/// 不会发给客户端。
async fn wait_first_keyframe(
    codec: VideoCodec,
    rx: &mut broadcast::Receiver<Bytes>,
    wait: Duration,
) -> FirstKeyframe {
    let start = Instant::now();
    loop {
        let remaining = wait.saturating_sub(start.elapsed());
        match timeout(remaining, rx.recv()).await {
            // 等待关键帧超时: 不再等待, 直接从 GOP 中间开始输出。
            Err(_) => return FirstKeyframe::Timeout,
            Ok(Err(broadcast::error::RecvError::Closed)) => return FirstKeyframe::Closed,
            // 落后于广播环: 错过的块直接跳过 (直播语义)。
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                eprintln!("http-ts: client lagged, skipped {n} chunk(s)");
            }
            Ok(Ok(data)) => {
                if chunk_is_video_keyframe(codec, &data) {
                    return FirstKeyframe::Found(data);
                }
            }
        }
    }
}

/// 把广播流搬到客户端 body 通道: 首包 (关键帧) 起头, 落后的块跳过。
/// 客户端断开或流结束时通道关闭, body 随之结束。
async fn pump(
    first: FirstKeyframe,
    mut rx: broadcast::Receiver<Bytes>,
    tx: mpsc::Sender<Bytes>,
) {
    if let FirstKeyframe::Found(data) = first {
        if tx.send(data).await.is_err() {
            return;
        }
    }
    loop {
        match rx.recv().await {
            Ok(data) => {
                if tx.send(data).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("http-ts: client lagged, skipped {n} chunk(s)");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// 绑定 `addr` 并在独立线程启动 HTTP 服务 (single-thread tokio runtime)。
/// 绑定失败立即报错返回。返回 Mux 输出 (实现 [`Write`]) 与实际绑定地址。
pub fn start(
    addr: &str,
    codec: VideoCodec,
) -> Result<(Box<dyn Write>, std::net::SocketAddr), String> {
    start_for(addr, codec, KEYFRAME_WAIT)
}

fn start_for(
    addr: &str,
    codec: VideoCodec,
    keyframe_wait: Duration,
) -> Result<(Box<dyn Write>, std::net::SocketAddr), String> {
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| format!("http-ts: bind {addr}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("http-ts: {e}"))?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;

    let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
    let state = AppState {
        codec,
        keyframe_wait,
        tx: tx.clone(),
    };
    let app = Router::new()
        .route("/live.ts", get(stream))
        .fallback(not_found)
        .with_state(state);

    std::thread::Builder::new()
        .name("http-ts".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("http-ts: build runtime");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener)
                    .expect("http-ts: wrap listener");
                let svc = app.into_make_service_with_connect_info::<SocketAddr>();
                if let Err(e) = axum::serve(listener, svc).await {
                    eprintln!("http-ts: server error: {e}");
                }
            });
        })
        .map_err(|e| format!("http-ts: spawn server thread: {e}"))?;

    eprintln!("http-ts: serving on http://{local}/live.ts");
    Ok((Box::new(Broadcaster { tx }), local))
}

async fn stream(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    // 先订阅再等关键帧: 等待期间到达的块不会错过订阅窗口。
    let mut rx = state.tx.subscribe();
    eprintln!(
        "http-ts: client {peer} connected ({} active), waiting up to {:?} for keyframe",
        state.tx.receiver_count(),
        state.keyframe_wait
    );
    let first = wait_first_keyframe(state.codec, &mut rx, state.keyframe_wait).await;
    if matches!(first, FirstKeyframe::Closed) {
        // 等待期间流结束 (会话已断)。
        eprintln!("http-ts: {peer} stream closed while waiting");
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("stream is not running\n"))
            .expect("static response");
    }
    match &first {
        FirstKeyframe::Found(_) => {
            eprintln!("http-ts: {peer} stream start (keyframe, {} active)", state.tx.receiver_count())
        }
        FirstKeyframe::Timeout => {
            eprintln!("http-ts: {peer} stream start (timeout, {} active)", state.tx.receiver_count())
        }
        FirstKeyframe::Closed => {}
    }

    let (tx, rx_body) = mpsc::channel::<Bytes>(CLIENT_BUFFER_CHUNKS);
    tokio::spawn(pump(first, rx, tx));
    let body =
        Body::from_stream(ReceiverStream::new(rx_body).map(Ok::<Bytes, std::convert::Infallible>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "video/mp2t")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .expect("static response headers")
}

async fn not_found(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    eprintln!("http-ts: {peer} requested {}, closing", req.uri().path());
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONNECTION, "close")
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("not found; the live stream is at /live.ts\n"))
        .expect("static response")
}

/// 检测一次 mux flush 的 TS 块里是否含视频关键帧: 提取视频 PID 的 PES
/// 载荷 (ES), 在 Annex B NAL 中找可随机访问的 VCL 单元。检测思路参考
/// rtmp-forwarder 的 stream::detect (那里面向 FLV tag), 这里面向 MPEG-TS;
/// 关键帧集合与 mux 的 classify 保持一致 (H.265 的 BLA/CRA 也可随机访问)。
fn chunk_is_video_keyframe(codec: VideoCodec, chunk: &[u8]) -> bool {
    let mut es: Vec<u8> = Vec::new();
    let mut i = 0;
    while i + TsPacket::SIZE <= chunk.len() {
        let p = &chunk[i..i + TsPacket::SIZE];
        i += TsPacket::SIZE;
        if p[0] != 0x47 {
            continue; // 防御: 非 TS 包
        }
        let pid = ((u16::from(p[1]) & 0x1f) << 8) | u16::from(p[2]);
        if pid != VIDEO_PID {
            continue;
        }
        let pusi = p[1] & 0x40 != 0;
        let afc = (p[3] >> 4) & 0b11;
        if afc & 0b01 == 0 {
            continue; // 无载荷 (仅 adaptation / 保留值)
        }
        let mut off = 4;
        if afc == 0b11 {
            off = 5 + usize::from(p[4]); // adaptation_field_length
            if off >= TsPacket::SIZE {
                continue;
            }
        }
        if pusi {
            // 一块内出现第二个 PES: 先判定已收集的部分再重来 (防御)。
            if annexb_has_key_nal(codec, &es) {
                return true;
            }
            es.clear();
            // PES 头: 00 00 01 sid len(2) flags(2) hdr_len(1) [可选头] ES
            if off + 9 > TsPacket::SIZE || p[off..off + 3] != [0, 0, 1] {
                continue;
            }
            let es_start = off + 9 + usize::from(p[off + 8]);
            if es_start > TsPacket::SIZE {
                continue;
            }
            es.extend_from_slice(&p[es_start..]);
        } else if !es.is_empty() {
            es.extend_from_slice(&p[off..]);
        }
    }
    annexb_has_key_nal(codec, &es)
}

/// Annex B 字节流中是否存在可随机访问的 VCL NAL:
/// H.264 = IDR (type 5); H.265 = BLA/CRA/IDR (16..=21, 与 mux 一致)。
fn annexb_has_key_nal(codec: VideoCodec, es: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            if let Some(&b) = es.get(i + 3) {
                let key = match codec {
                    VideoCodec::H264 => b & 0x1f == 5,
                    VideoCodec::H265 => {
                        let t = (b >> 1) & 0x3f;
                        (16..=21).contains(&t)
                    }
                };
                if key {
                    return true;
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::Mux;
    use std::io::Read as _;
    use std::net::TcpStream;

    // 与 mux::tests 相同的合成 AU: SPS + PPS + IDR。
    fn sample_h264() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E];
        au.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x38, 0x80]);
        au.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0xA0]);
        au
    }

    fn sample_h265() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1, 0x42, 0x01, 0xAA]; // VPS
        au.extend_from_slice(&[0, 0, 1, 0x44, 0x01, 0xBB]); // SPS
        au.extend_from_slice(&[0, 0, 1, 0x46, 0x01, 0xCC]); // PPS
        au.extend_from_slice(&[0, 0, 1, 0x26, 0x01, 0xDD]); // IDR_W_RADL
        au
    }

    /// 用真 mux 造一次 flush 的 TS 块: key = SPS+PPS+IDR, 否则 P 帧。
    fn ts_chunk(key: bool) -> Vec<u8> {
        let mut mux = Mux::new(Vec::new(), VideoCodec::H264, 60).unwrap();
        if key {
            mux.push_video(&sample_h264(), 0).unwrap();
        } else {
            mux.push_video(&[0, 0, 1, 0x41, 0x9A, 0xBC], 0).unwrap();
        }
        mux.out.into_inner().unwrap()
    }

    /// 一块 TS 字节在 HTTP chunked 传输后的线上长度:
    /// `{hex(len)}\r\n` + 数据 + `\r\n`。
    fn framed(len: usize) -> usize {
        len + format!("{len:x}").len() + 4
    }

    /// 发 GET (不读响应)。
    fn send_get(addr: std::net::SocketAddr, path: &str) -> TcpStream {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c
    }

    /// 读走响应头, 返回头部之后残留的字节 (可能已含背靠背到达的 body 数据)。
    fn read_head(c: &mut TcpStream, expect_status: &str) -> Vec<u8> {
        let mut buf = [0u8; 8192];
        let mut all: Vec<u8> = Vec::new();
        loop {
            let n = c.read(&mut buf).unwrap();
            assert!(n > 0, "closed before response headers");
            all.extend_from_slice(&buf[..n]);
            if all.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&all);
        assert!(head.starts_with(expect_status), "{head}");
        if expect_status == "HTTP/1.1 200" {
            let lower = head.to_ascii_lowercase();
            assert!(lower.contains("content-type: video/mp2t"), "{head}");
            assert!(lower.contains("transfer-encoding: chunked"), "{head}");
        }
        let pos = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        all.split_off(pos)
    }

    #[test]
    fn detect_keyframe_chunks() {
        assert!(chunk_is_video_keyframe(VideoCodec::H264, &ts_chunk(true)));
        assert!(!chunk_is_video_keyframe(VideoCodec::H264, &ts_chunk(false)));
        // 非 TS 垃圾与空块。
        assert!(!chunk_is_video_keyframe(VideoCodec::H264, &[0x47; 188 * 4]));
        assert!(!chunk_is_video_keyframe(VideoCodec::H264, &[]));
        // H.265: VPS+SPS+PPS+IDR_W_RADL(19)。
        let mut mux = Mux::new(Vec::new(), VideoCodec::H265, 60).unwrap();
        mux.push_video(&sample_h265(), 0).unwrap();
        let h265 = mux.out.into_inner().unwrap();
        assert!(chunk_is_video_keyframe(VideoCodec::H265, &h265));
        // 用 H.264 规则判定同一条 h265 块: 不应识别为关键帧。
        assert!(!chunk_is_video_keyframe(VideoCodec::H264, &h265));
    }

    #[test]
    fn client_waits_for_keyframe() {
        let (mut sink, addr) = start("127.0.0.1:0", VideoCodec::H264).unwrap();
        let mut c1 = send_get(addr, "/live.ts");
        let mut buf = [0u8; 65536];

        let key = ts_chunk(true);
        let inter = ts_chunk(false);

        // 服务端此时在等关键帧: 先到的非关键帧块被丢弃, 不进入响应。
        sink.write_all(&inter).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // 给订阅留出时间后广播关键帧: 响应头返回, 首包就是这个关键帧块。
        std::thread::sleep(Duration::from_millis(100));
        sink.write_all(&key).unwrap();
        let leftover = read_head(&mut c1, "HTTP/1.1 200");

        // 之后的块照常下发。
        sink.write_all(&inter).unwrap();
        let expect = framed(key.len()) + framed(inter.len());
        let mut got = leftover.len();
        while got < expect {
            let n = c1.read(&mut buf).unwrap();
            assert!(n > 0, "closed while streaming");
            got += n;
        }
        assert_eq!(got, expect, "must start exactly at the keyframe chunk");
    }

    #[test]
    fn client_timeout_starts_stream() {
        let (mut sink, addr) =
            start_for("127.0.0.1:0", VideoCodec::H264, Duration::from_millis(300))
                .unwrap();
        let mut c1 = send_get(addr, "/live.ts");
        let mut buf = [0u8; 65536];

        let inter = ts_chunk(false);
        // 窗口内的块: 被等待循环丢弃。
        sink.write_all(&inter).unwrap();
        std::thread::sleep(Duration::from_millis(500)); // 超过 300ms 窗口
        // 超时后响应头返回; 窗口外的块直接下发。
        let leftover = read_head(&mut c1, "HTTP/1.1 200");
        sink.write_all(&inter).unwrap();
        let expect = framed(inter.len());
        let mut got = leftover.len();
        while got < expect {
            let n = c1.read(&mut buf).unwrap();
            assert!(n > 0, "closed while streaming");
            got += n;
        }
        assert_eq!(got, expect, "only post-timeout chunk must arrive");
    }

    /// 端到端冒烟: 真实 HTTP 栈上的状态行 / 响应头 / 404 / 流式送达。
    #[test]
    fn http_serves_stream_and_404() {
        let (mut sink, addr) = start("127.0.0.1:0", VideoCodec::H264).unwrap();
        let mut buf = [0u8; 8192];

        // "/" 与未知路径 -> 404 并断开。
        for path in ["/", "/nope"] {
            let mut c0 = send_get(addr, path);
            let mut all404: Vec<u8> = Vec::new();
            loop {
                let n = c0.read(&mut buf).unwrap();
                assert!(n > 0, "closed before 404 response");
                all404.extend_from_slice(&buf[..n]);
                if all404.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(
                String::from_utf8_lossy(&all404).starts_with("HTTP/1.1 404"),
                "path {path} should be rejected"
            );
        }

        // 流路径: 广播一个关键帧块 (放行并作为首包), 之后 30 个 P 帧块
        // 16ms 间隔, 并发读者应实时收齐全部字节。
        let key = ts_chunk(true);
        let inter = ts_chunk(false);
        let expect = framed(key.len()) + 30 * framed(inter.len());
        let mut c1 = send_get(addr, "/live.ts");
        // 给订阅留出时间 (否则关键帧块可能广播在订阅之前而被错过)。
        std::thread::sleep(Duration::from_millis(100));
        sink.write_all(&key).unwrap();
        let leftover = read_head(&mut c1, "HTTP/1.1 200");
        let reader = std::thread::spawn(move || {
            let t0 = Instant::now();
            let mut got = leftover.len();
            let mut first = None;
            let mut rbuf = [0u8; 65536];
            while got < expect {
                match c1.read(&mut rbuf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if first.is_none() {
                            first = Some(t0.elapsed());
                        }
                        got += n;
                    }
                    Err(_) => break,
                }
            }
            (got, first)
        });
        for _ in 0..30 {
            sink.write_all(&inter).unwrap();
            std::thread::sleep(Duration::from_millis(16));
        }
        let (got, first) = reader.join().unwrap();
        assert_eq!(got, expect, "stream truncated or extra data");
        assert!(
            first.is_some_and(|d| d < Duration::from_millis(500)),
            "first frame arrived at {first:?}, streaming not real-time"
        );
    }
}
