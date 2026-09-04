//! chiaki-stream: PS Remote Play 注册 / 连接, 音视频直通封装为 MPEG-TS。
//!
//! - 音频: Opus 透传 (不解码不转码), TS 里 stream_type 0x06 + Opus 注册描述子。
//! - 视频: H.264/H.265 Annex B 直通。
//! - 时间戳: 到达时刻的 wall clock (90kHz), 音视频同钟, 天然同步。
//! - 日志全部走 stderr; `--output -` 时 stdout 只写 TS 数据。

mod args;
mod creds;
mod hosts;
mod mux;
mod run;

use args::{Cli, Cmd};
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    let level = cli.log_level;
    let r = match &cli.cmd {
        Cmd::Regist(a) => run::cmd_regist(a, level),
        Cmd::Stream(a) => run::cmd_stream(a, level),
        Cmd::Wakeup(a) => run::cmd_wakeup(a, level),
        Cmd::List => run::cmd_list(),
    };
    if let Err(e) = r {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
