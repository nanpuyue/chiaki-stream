//! regist / stream / wakeup 流程。

use std::fs::File;
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use libchiaki::{
    ConnectInfo, Discovery, Event, Log, LogLevel, Regist, RegistEvent, Session, VideoSample, ffi,
    lib_init, log_level_char, LOG_ALL,
};

use crate::args::{
    CodecArg, ConsoleArg, FpsArg, LogLevelArg, RegistArgs, ResArg, StreamArgs, WakeupArgs,
};
use crate::creds::{
    expand_regist_key_text as exp_regist_key, parse_account_id, parse_hex16, parse_target,
    regist_key_text, to_hex, wakeup_credential,
};
use crate::mux::{Mux, VideoCodec};

type Res<T> = Result<T, String>;

/// 日志级别 -> chiaki 库日志掩码 (选择该级别及更严重级别, 高级别覆盖低级别)。
fn log_mask(level: LogLevelArg) -> u32 {
    use LogLevelArg::*;
    let e = LogLevel::CHIAKI_LOG_ERROR as u32;
    let w = LogLevel::CHIAKI_LOG_WARNING as u32;
    let i = LogLevel::CHIAKI_LOG_INFO as u32;
    let v = LogLevel::CHIAKI_LOG_VERBOSE as u32;
    match level {
        Off => 0,
        Error => e,
        Warning => w | e,
        Info => i | w | e,
        Verbose => v | i | w | e,
        Debug => LOG_ALL,
    }
}

fn new_log(level: LogLevelArg) -> Log {
    Log::new(log_mask(level), |lvl, msg| {
        eprintln!("[{}] {}", log_level_char(lvl), msg);
    })
}

fn is_ps5(c: ConsoleArg) -> bool {
    matches!(c, ConsoleArg::Ps5)
}

// ---------------------------------------------------------------------------
// 默认分辨率 / 比特率
// ---------------------------------------------------------------------------

/// PS4 Pro 的 target 值。
const PS4_PRO_TARGET: u32 = 900;

/// 根据主机类型返回默认分辨率: PS4 -> 720p, PS4 Pro / PS5 -> 1080p。
fn default_resolution(ps5: bool, target: u32) -> ResArg {
    if ps5 || target == PS4_PRO_TARGET {
        ResArg::P1080
    } else {
        ResArg::P720
    }
}

/// 根据分辨率返回默认比特率 (kbps)。
fn default_bitrate(res: ResArg) -> u32 {
    match res {
        ResArg::P360 | ResArg::P540 | ResArg::P720 => 8000,
        ResArg::P1080 => 10000,
    }
}

fn map_res(r: ResArg) -> libchiaki::ResolutionPreset {
    use ffi::ChiakiVideoResolutionPreset::*;
    match r {
        ResArg::P360 => CHIAKI_VIDEO_RESOLUTION_PRESET_360p,
        ResArg::P540 => CHIAKI_VIDEO_RESOLUTION_PRESET_540p,
        ResArg::P720 => CHIAKI_VIDEO_RESOLUTION_PRESET_720p,
        ResArg::P1080 => CHIAKI_VIDEO_RESOLUTION_PRESET_1080p,
    }
}

fn map_fps(f: FpsArg) -> libchiaki::FpsPreset {
    use ffi::ChiakiVideoFPSPreset::*;
    match f {
        FpsArg::F30 => CHIAKI_VIDEO_FPS_PRESET_30,
        FpsArg::F60 => CHIAKI_VIDEO_FPS_PRESET_60,
    }
}

fn res_label(r: ResArg) -> &'static str {
    match r {
        ResArg::P360 => "360p",
        ResArg::P540 => "540p",
        ResArg::P720 => "720p",
        ResArg::P1080 => "1080p",
    }
}

// ---------------------------------------------------------------------------
// regist
// ---------------------------------------------------------------------------

pub fn cmd_regist(a: &RegistArgs, level: LogLevelArg) -> Res<()> {
    if a.account_id.is_none() && a.online_id.is_none() {
        return Err(
            "regist requires at least one PSN identity: --account-id <16-hex> or --online-id <name>"
                .to_string(),
        );
    }
    lib_init().map_err(|e| e.to_string())?;
    let log = new_log(level);
    let target = parse_target(Some(&a.target), false)?;
    let ps5 = libchiaki::common::target_is_ps5(target);
    let mut info =
        libchiaki::RegistInfo::new(target, &a.host, a.pin).map_err(|e| e.to_string())?;
    if let Some(acc) = &a.account_id {
        info.set_psn_account_id(&parse_account_id(acc)?);
    }
    if let Some(oid) = &a.online_id {
        info.set_psn_online_id(oid).map_err(|e| e.to_string())?;
    }
    info.set_console_pin(a.console_pin);

    let (tx, rx) = mpsc::channel();
    let _regist =
        Regist::start(&log, info, move |ev| {
            let _ = tx.send(ev);
        })
        .map_err(|e| e.to_string())?;
    eprintln!("registering {} ... (wait up to 120s)", a.host);
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(RegistEvent::FinishedSuccess(h)) => {
            let console = if ps5 { "ps5" } else { "ps4" };
            println!(
                "{}: --host={} --console={} --regist-key={} --morning={}",
                h.server_nickname,
                a.host,
                console,
                regist_key_text(&h.rp_regist_key),
                to_hex(&h.rp_key),
            );
            Ok(())
        }
        Ok(other) => Err(format!("regist failed: {other:?}")),
        Err(_) => Err("regist timed out after 120s".to_string()),
    }
    // _regist Drop 时 stop + fini。
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn cmd_list() -> Res<()> {
    let hosts = crate::hosts::read_chiaki_hosts()?;
    for h in &hosts {
        let console = if h.ps5 { "ps5" } else { "ps4" };
        let ip = h.host_ip.as_deref().unwrap_or("-");
        println!(
            "{}: --host={} --console={} --regist-key={} --morning={}",
            h.nickname,
            ip,
            console,
            regist_key_text(&h.regist_key),
            to_hex(&h.morning),
        );
    }
    eprintln!("{} host(s)", hosts.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// stream
// ---------------------------------------------------------------------------

enum Msg {
    Video(Vec<u8>, i32),
    Audio(Vec<u8>),
    AudioFormat { rate: u32, frame_size: u32 },
}

pub fn cmd_stream(a: &StreamArgs, level: LogLevelArg) -> Res<()> {
    let (host, ps5, target, rk, morning) = if let Some(q) = &a.host_query {
        let hosts = crate::hosts::read_chiaki_hosts()?;
        let h = crate::hosts::find_host(&hosts, q)?;
        let host = a.host.clone().or(h.host_ip.clone()).ok_or(
            "registry has no IP for this host (DHCP changed?), pass --host explicitly"
                .to_string(),
        )?;
        eprintln!(
            "using chiaki-ng host '{}' ip={host} ps5={} target={}",
            h.nickname, h.ps5, h.target
        );
        (host, h.ps5, h.target, h.regist_key, h.morning)
    } else {
        let host = a.host.clone().ok_or("--host is required")?;
        let console = a.console.ok_or("--console is required (ps4|ps5)")?;
        let rk = exp_regist_key(&a.regist_key.clone().ok_or("--regist-key is required")?)?;
        let morning = parse_hex16(&a.morning.clone().ok_or("--morning is required")?)?;
        let target = if is_ps5(console) { 1000100 } else { 1000 };
        (host, is_ps5(console), target, rk, morning)
    };

    lib_init().map_err(|e| e.to_string())?;
    let log = new_log(level);

    // 分辨率: CLI 显式指定 > 按主机类型默认。
    let resolution = a.resolution.unwrap_or_else(|| default_resolution(ps5, target));
    let bitrate = a.bitrate.unwrap_or_else(|| default_bitrate(resolution));

    eprintln!(
        "video: {} {} {}kbps codec={}",
        res_label(resolution),
        match a.fps {
            FpsArg::F30 => "30fps",
            FpsArg::F60 => "60fps",
        },
        bitrate,
        match a.codec {
            CodecArg::H264 => "h264",
            CodecArg::H265 => "h265",
            CodecArg::H265Hdr => "h265-hdr",
        },
    );

    let mut info = ConnectInfo::new(&host, &rk, ps5).map_err(|e| e.to_string())?;
    info.set_morning(&morning);
    if let Some(acc) = &a.account_id {
        info.set_psn_account_id(&parse_account_id(acc)?);
    }
    info.set_video_preset(map_res(resolution), map_fps(a.fps));
    info.set_bitrate(bitrate);

    let vcodec = match a.codec {
        CodecArg::H264 => {
            info.set_video_codec(ffi::ChiakiCodec::CHIAKI_CODEC_H264);
            VideoCodec::H264
        }
        CodecArg::H265 => {
            info.set_video_codec(ffi::ChiakiCodec::CHIAKI_CODEC_H265);
            VideoCodec::H265
        }
        CodecArg::H265Hdr => {
            info.set_video_codec(ffi::ChiakiCodec::CHIAKI_CODEC_H265_HDR);
            VideoCodec::H265
        }
    };

    let mut session = Session::new(info, &log).map_err(|e| e.to_string())?;

    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    let (media_tx, media_rx) = mpsc::sync_channel::<Msg>(48);

    session.set_event_callback(move |ev| {
        let _ = ev_tx.send(ev);
    });

    // 视频: 拷贝出 C 缓冲, 拥塞时丢帧并返回 false (chiaki 会补 IDR)。
    let vtx = media_tx.clone();
    let mut vdropped = 0u64;
    session.set_video_sample_callback(move |s: VideoSample| {
        match vtx.try_send(Msg::Video(s.data.to_vec(), s.frames_lost)) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                vdropped += 1;
                if vdropped % 60 == 1 {
                    eprintln!("video congested, dropped {vdropped} frames");
                }
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    });

    // 音频: Opus 透传, 只拷贝。header 信息经同一 channel 发给 mux
    // (保序), 用于精确包时长。
    let atx = media_tx.clone();
    let ftx = media_tx.clone();
    drop(media_tx);
    let mut adropped = 0u64;
    let mut audio_logged = false;
    session.set_audio_sink(
        move |h: &mut libchiaki::AudioHeader| {
            if !audio_logged {
                audio_logged = true;
                eprintln!(
                    "audio: {}ch {}bits {}Hz frame={}",
                    h.0.channels, h.0.bits, h.0.rate, h.0.frame_size
                );
            }
            let _ = ftx.try_send(Msg::AudioFormat {
                rate: h.0.rate,
                frame_size: h.0.frame_size,
            });
        },
        move |data: &mut [u8]| {
            if data.is_empty() {
                return;
            }
            if atx.try_send(Msg::Audio(data.to_vec())).is_err() {
                adropped += 1;
                if adropped % 300 == 1 {
                    eprintln!("audio congested, dropped {adropped} packets");
                }
            }
        },
    );

    session.start().map_err(|e| e.to_string())?;
    eprintln!("session started, waiting for connect...");

    let out: Box<dyn Write> = if a.output == "-" {
        Box::new(std::io::stdout())
    } else {
        Box::new(File::create(&a.output).map_err(|e| e.to_string())?)
    };
    let fps = match a.fps {
        FpsArg::F30 => 30,
        FpsArg::F60 => 60,
    };
    let mut mux = Mux::new(out, vcodec, fps).map_err(|e| e.to_string())?;

    let login_pin = a.login_pin.clone();
    let idr_interval = Duration::from_secs_f64(a.idr_interval);
    let mut last_idr: Option<Instant> = None;
    loop {
        while let Ok(msg) = media_rx.try_recv() {
            let r = match msg {
                Msg::Video(b, lost) => mux.push_video(&b, lost),
                Msg::Audio(b) => mux.push_audio(&b),
                Msg::AudioFormat { rate, frame_size } => {
                    mux.set_audio_format(rate, frame_size);
                    Ok(())
                }
            };
            if let Err(e) = r {
                eprintln!("write error: {e}");
                return Ok(());
            }
        }
        match ev_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Connected) => {
                eprintln!("connected, streaming");
                last_idr = Some(Instant::now());
            }
            Ok(Event::LoginPinRequest { pin_incorrect }) => {
                if pin_incorrect {
                    eprintln!("login PIN was incorrect");
                }
                match &login_pin {
                    Some(pin) => session
                        .set_login_pin(pin.as_bytes())
                        .map_err(|e| e.to_string())?,
                    None => {
                        return Err(
                            "console asks for login PIN (use --login-pin)".to_string()
                        )
                    }
                }
            }
            Ok(Event::Quit { reason_str, .. }) => {
                eprintln!("quit: {reason_str}");
                break;
            }
            Ok(ev) => eprintln!("event: {ev:?}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("event channel closed");
                break;
            }
        }
        if !idr_interval.is_zero() {
            if let Some(t) = last_idr {
                if t.elapsed() >= idr_interval {
                    let _ = session.request_idr();
                    last_idr = Some(Instant::now());
                }
            }
        }
    }

    let _ = session.stop();
    let _ = session.join();
    if let Err(e) = mux.flush() {
        eprintln!("write error: {e}");
        return Ok(());
    }
    eprintln!("done");
    Ok(())
}

// ---------------------------------------------------------------------------
// wakeup
// ---------------------------------------------------------------------------

pub fn cmd_wakeup(a: &WakeupArgs, level: LogLevelArg) -> Res<()> {
    lib_init().map_err(|e| e.to_string())?;
    let log = new_log(level);
    let rk = exp_regist_key(&a.regist_key)?;
    let cred = wakeup_credential(&rk)?;
    Discovery::wakeup(&log, None, &a.host, cred, is_ps5(a.console))
        .map_err(|e| e.to_string())?;
    eprintln!("wakeup sent to {}", a.host);
    Ok(())
}
