//! 命令行参数 (clap derive)。

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "chiaki-stream",
    version,
    about = "PS Remote Play: 注册 / 连接, 音视频直通封装为 MPEG-TS (不转码)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// 配对注册 (PIN 显示在主机上), 成功后打印可复制的 --regist-key / --morning 参数。
    Regist(RegistArgs),
    /// 连接主机, 音视频封装为 MPEG-TS 输出。
    Stream(StreamArgs),
    /// 唤醒待机中的主机。
    Wakeup(WakeupArgs),
    /// 列出官方 chiaki-ng 已注册的主机 (仅 Windows)。
    List,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum ConsoleArg {
    Ps4,
    Ps5,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum ResArg {
    #[value(name = "360p")]
    P360,
    #[value(name = "540p")]
    P540,
    #[value(name = "720p")]
    P720,
    #[value(name = "1080p")]
    P1080,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum FpsArg {
    #[value(name = "30")]
    F30,
    #[value(name = "60")]
    F60,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum CodecArg {
    #[value(name = "h264")]
    H264,
    #[value(name = "h265")]
    H265,
    #[value(name = "h265-hdr")]
    H265Hdr,
}

#[derive(Args, Debug)]
pub struct RegistArgs {
    /// 主机 IP。
    #[arg(long)]
    pub host: String,
    /// 主机上显示的 8 位配对 PIN。
    #[arg(long)]
    pub pin: u32,
    /// PSN Account ID, 与 chiaki-ng 要求相同 , PS4 GoldHEN 本地用户填 fffffffffff=。
    #[arg(long)]
    pub account_id: Option<String>,
    /// PSN Online ID, 仅用于旧版 (PS4-1/PS4-2) 可替代 account-id。
    #[arg(long)]
    pub online_id: Option<String>,
    /// 主机家长控制 PIN, 默认 0。
    #[arg(long, default_value_t = 0)]
    pub console_pin: u32,
    /// 注册目标版本。可传编号或别名。
    #[arg(long, help = "注册目标版本 (必填)。可传编号或别名:\n\
        1: PS4 Firmware <  7.0         (PS4-1)\n\
        2: PS4 Firmware >= 7.0, < 8.0  (PS4-2)\n\
        3: PS4 Firmware >= 8.0         (PS4-3)\n\
        4: PS5                         (PS5)")]
    pub target: String,
}

#[derive(Args, Debug)]
pub struct StreamArgs {
    /// 主机昵称/IP/MAC: 从官方 chiaki-ng 注册表读取并直连。
    /// 给了它就不用再给 --host/--console/--regist-key/--morning。
    #[arg(index = 1, value_name = "NICKNAME")]
    pub host_query: Option<String>,
    /// 主机 IP (手动模式必填; 注册表模式下可覆盖)。
    #[arg(long)]
    pub host: Option<String>,
    /// 主机类型 (手动模式必填)。
    #[arg(long, value_enum)]
    pub console: Option<ConsoleArg>,
    /// regist-key 文本 (如 "4a163489", 即 regist 命令输出)。
    #[arg(long)]
    pub regist_key: Option<String>,
    /// morning (rp_key), 32 个 hex 字符。
    #[arg(long)]
    pub morning: Option<String>,
    /// PSN AccountID, 16 个 hex 字符或 base64 (可选)。
    #[arg(long)]
    pub account_id: Option<String>,
    /// 登录 PIN (主机要求登录 PIN 时用)。
    #[arg(long)]
    pub login_pin: Option<String>,
    #[arg(long, value_enum)]
    pub resolution: Option<ResArg>,
    #[arg(long, value_enum, default_value = "30")]
    pub fps: FpsArg,
    #[arg(long, value_enum, default_value = "h264")]
    pub codec: CodecArg,
    /// 比特率 (kbps)。不指定则按分辨率自动 (720p=8000, 1080p=10000)。
    #[arg(long)]
    pub bitrate: Option<u32>,
    /// 输出文件, "-" 表示 stdout (日志一律走 stderr)。
    #[arg(long, default_value = "-")]
    pub output: String,
}

#[derive(Args, Debug)]
pub struct WakeupArgs {
    /// 主机 IP。
    #[arg(long)]
    pub host: String,
    #[arg(long, value_enum)]
    pub console: ConsoleArg,
    /// regist-key 文本 (如 "4a163489")。
    #[arg(long)]
    pub regist_key: String,
}
