//! MPEG-TS 封装: H.264/H.265 Annex B 直通 + Opus 透传 (不转码)。
//!
//! TS/PES/PSI 序列化用 mpeg2ts crate; 本模块只做:
//! Annex B NAL 解析、SPS/PPS 缓存、关键帧判定、PTS 时钟策略、PID/CC 编排。

use std::io::{BufWriter, Write};
use std::time::Instant;

use mpeg2ts::es::{StreamId, StreamType};
use mpeg2ts::pes::PesHeader;
use mpeg2ts::time::{ClockReference, Timestamp};
use mpeg2ts::ts::payload::{Bytes, Pat, Pes, Pmt};
use mpeg2ts::ts::{
    AdaptationField, ContinuityCounter, Descriptor, EsInfo, Pid, ProgramAssociation,
    TransportScramblingControl, TsHeader, TsPacket, TsPacketWriter, TsPayload, VersionNumber,
    WriteTsPacket,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoCodec {
    H264,
    H265,
}

const PMT_PID: u16 = 0x100;
const VIDEO_PID: u16 = 0x101;
const AUDIO_PID: u16 = 0x102;

/// PES 头固定 14 字节 (9 固定 + 5 PTS, 仅 PTS 无 DTS)。
const PES_HDR_LEN: usize = 14;

fn pid(n: u16) -> Pid {
    Pid::new(n).expect("pid")
}

fn ts_now(t0: &mut Option<Instant>) -> u64 {
    let t0 = *t0.get_or_insert_with(Instant::now);
    let us = t0.elapsed().as_micros() as u64;
    (us * 90 / 1000) & Timestamp::MAX
}

// ---------------------------------------------------------------------------
// Annex B NAL 解析
// ---------------------------------------------------------------------------

/// 切分 Annex B AU, 每个单元保留自己的起始码 (3 或 4 字节原样保留)。
/// 空单元 (纯起始码) 会被跳过。
///
/// 扫描约定: 在首个 0 处匹配 `00 00 01`; 若其前还有一个 0,
/// 则为 4 字节起始码 (起始位置再往前一个)。
pub fn split_annexb(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let start = if i >= 1 && au[i - 1] == 0 { i - 1 } else { i };
            starts.push(start);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let e = if k + 1 < starts.len() {
            starts[k + 1]
        } else {
            au.len()
        };
        // 跳过纯起始码的空单元。
        let code_len = if au[s..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            3
        };
        if e - s > code_len {
            out.push(&au[s..e]);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NalKind {
    Vcl { key: bool },
    ParamSet,
    Other,
}

fn nal_unit_type(codec: VideoCodec, nal: &[u8]) -> Option<u8> {
    let code_len = if nal.starts_with(&[0, 0, 0, 1]) {
        4
    } else if nal.starts_with(&[0, 0, 1]) {
        3
    } else {
        return None;
    };
    let b = *nal.get(code_len)?;
    Some(match codec {
        VideoCodec::H264 => b & 0x1F,
        VideoCodec::H265 => (b >> 1) & 0x3F,
    })
}

fn classify(codec: VideoCodec, nal: &[u8]) -> NalKind {
    let Some(t) = nal_unit_type(codec, nal) else {
        return NalKind::Other;
    };
    match codec {
        VideoCodec::H264 => match t {
            1..=4 => NalKind::Vcl { key: false },
            5 => NalKind::Vcl { key: true },
            7 | 8 => NalKind::ParamSet,
            _ => NalKind::Other,
        },
        VideoCodec::H265 => match t {
            0..=9 => NalKind::Vcl { key: false },
            16..=21 => NalKind::Vcl { key: true },
            32..=34 => NalKind::ParamSet,
            _ => NalKind::Other,
        },
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

/// PTS 策略: 音视频共享 wall-clock 原点 (首包到达时刻),
/// 之后按计数推进 (视频: 帧数×帧间隔, 含 frames_lost;
/// 音频: 包数×标称包时长), 与 wall-clock 偏离超阈值时重定基
/// (处理 seek/停顿, 播放器重同步一次即可)。
const REBASE_VIDEO_TICKS: u64 = 45000; // 500ms
const REBASE_AUDIO_TICKS: u64 = 22500; // 250ms

pub struct Mux<W: Write> {
    pub(crate) out: BufWriter<W>,
    cc: [ContinuityCounter; 4], // pat, pmt, video, audio
    video_pid: Pid,
    audio_pid: Pid,
    pmt_pid: Pid,
    video_stream_type: StreamType,
    ps_cache: Vec<u8>,
    t0: Option<Instant>,
    frame_ticks: u64,
    video_pts: Option<u64>,
    audio_pts: Option<u64>,
    audio_ticks: u64,
}

impl<W: Write> Mux<W> {
    pub fn new(stream: W, codec: VideoCodec, fps: u32) -> mpeg2ts::Result<Self> {
        let video_stream_type = match codec {
            VideoCodec::H264 => StreamType::H264,
            VideoCodec::H265 => StreamType::H265,
        };
        Ok(Mux {
            out: BufWriter::with_capacity(1 << 20, stream),
            cc: [
                ContinuityCounter::new(),
                ContinuityCounter::new(),
                ContinuityCounter::new(),
                ContinuityCounter::new(),
            ],
            video_pid: pid(VIDEO_PID),
            audio_pid: pid(AUDIO_PID),
            pmt_pid: pid(PMT_PID),
            video_stream_type,
            ps_cache: Vec::new(),
            t0: None,
            frame_ticks: 90000 / fps.max(1) as u64,
            video_pts: None,
            audio_pts: None,
            audio_ticks: 900, // 480 samples @48k, 等首个 header 纠正
        })
    }

    fn next_cc(&mut self, idx: usize) -> ContinuityCounter {
        let c = self.cc[idx];
        self.cc[idx].increment();
        c
    }

    /// 写一个 TS 包。短载荷由 mpeg2ts 自动用 adaptation stuffing 补齐,
    /// payload_unit_start 由载荷类型自动推导 (PesStart 为 true)。
    fn write_packet(
        &mut self,
        pid: Pid,
        cc_idx: usize,
        adapt: Option<AdaptationField>,
        payload: TsPayload,
    ) -> mpeg2ts::Result<()> {
        let cc = self.next_cc(cc_idx);
        let pkt = TsPacket {
            header: TsHeader {
                transport_error_indicator: false,
                transport_priority: false,
                pid,
                transport_scrambling_control: TransportScramblingControl::NotScrambled,
                continuity_counter: cc,
            },
            adaptation_field: adapt,
            payload: Some(payload),
        };
        // TsPacketWriter 不暴露底层流, 每次借用构造 (无状态, 开销可忽略)。
        let mut w = TsPacketWriter::new(&mut self.out);
        w.write_ts_packet(&pkt)
    }

    fn pat(&self) -> Pat {
        Pat {
            transport_stream_id: 1,
            version_number: VersionNumber::new(),
            table: vec![ProgramAssociation {
                program_num: 1,
                program_map_pid: self.pmt_pid,
            }],
        }
    }

    fn pmt(&self) -> Pmt {
        Pmt {
            program_num: 1,
            pcr_pid: Some(self.video_pid),
            version_number: VersionNumber::new(),
            program_info: vec![],
            es_info: vec![
                EsInfo {
                    stream_type: self.video_stream_type,
                    elementary_pid: self.video_pid,
                    descriptors: vec![],
                },
                EsInfo {
                    stream_type: StreamType::Mpeg2PacketizedData,
                    elementary_pid: self.audio_pid,
                    descriptors: vec![Descriptor {
                        tag: 5,
                        data: b"Opus".to_vec(),
                    }],
                },
            ],
        }
    }

    fn emit_pat_pmt(&mut self) -> mpeg2ts::Result<()> {
        let pat = self.pat();
        self.write_packet(pid(0), 0, None, TsPayload::Pat(pat))?;
        let pmt = self.pmt();
        self.write_packet(self.pmt_pid, 1, None, TsPayload::Pmt(pmt))?;
        Ok(())
    }

    fn pes_header(&self, stream_id: u8, pts: u64) -> PesHeader {
        PesHeader {
            stream_id: StreamId::new(stream_id),
            priority: false,
            data_alignment_indicator: true,
            copyright: false,
            original_or_copy: false,
            pts: Some(Timestamp::new(pts).expect("pts")),
            dts: None,
            escr: None,
        }
    }

    /// 写一个 PES: 首包 PesStart (PES 头 + 首段数据, 可带 adaptation),
    /// 后续包 PesContinuation, 每包 184B 切分, 尾包自动 stuffing。
    fn write_pes(
        &mut self,
        pid: Pid,
        cc_idx: usize,
        stream_id: u8,
        pts: u64,
        payload: &[u8],
        adapt_first: Option<AdaptationField>,
    ) -> mpeg2ts::Result<()> {
        assert!(!payload.is_empty(), "empty PES payload");
        let adapt_len = match &adapt_first {
            None => 0,
            // len 字节 + flags 字节 + PCR/OPCR。
            Some(a) => {
                1 + 1 + if a.pcr.is_some() { 6 } else { 0 } + if a.opcr.is_some() { 6 } else { 0 }
            }
        };
        let first_cap = 184usize.saturating_sub(adapt_len + PES_HDR_LEN);
        assert!(first_cap > 0, "no room for PES data");
        // 首包。
        let n = payload.len().min(first_cap);
        let pes = Pes {
            header: self.pes_header(stream_id, pts),
            pes_packet_len: 0,
            data: Bytes::new(&payload[..n])?,
        };
        self.write_packet(pid, cc_idx, adapt_first, TsPayload::PesStart(pes))?;
        let mut off = n;
        // 后续包。
        while off < payload.len() {
            let n = (payload.len() - off).min(184);
            let data = Bytes::new(&payload[off..off + n])?;
            self.write_packet(pid, cc_idx, None, TsPayload::PesContinuation(data))?;
            off += n;
        }
        Ok(())
    }

    /// 音频标称包时长 (header 给出 rate/frame_size 后精确化)。
    pub fn set_audio_format(&mut self, rate: u32, frame_size: u32) {
        if rate > 0 {
            self.audio_ticks = frame_size as u64 * 90000 / rate as u64;
        }
    }

    /// 视频时间戳: 本帧 = 下一预期帧 + 丢失帧补偿, 然后推进一帧。
    /// 首帧用 wall-clock, 偏离超阈值重定基。
    fn video_stamp(&mut self, lost_frames: u64) -> u64 {
        let now = ts_now(&mut self.t0);
        let base = match self.video_pts {
            None => now,
            Some(next) => {
                let p = next + lost_frames * self.frame_ticks;
                if now.abs_diff(p) > REBASE_VIDEO_TICKS {
                    now
                } else {
                    p
                }
            }
        };
        self.video_pts = Some(base + self.frame_ticks);
        base
    }

    /// 音频时间戳 (到达包): 与视频同理, 无丢失计数。
    fn audio_stamp(&mut self) -> u64 {
        let step = self.audio_ticks;
        let now = ts_now(&mut self.t0);
        let base = match self.audio_pts {
            None => now,
            Some(next) => {
                if now.abs_diff(next) > REBASE_AUDIO_TICKS {
                    now
                } else {
                    next
                }
            }
        };
        self.audio_pts = Some(base + step);
        base
    }

    /// PLC 空包: 时间线照样推进, 不输出。
    fn audio_skip(&mut self) {
        if let Some(next) = self.audio_pts {
            self.audio_pts = Some(next + self.audio_ticks);
        }
    }

    pub fn push_video(&mut self, au: &[u8], frames_lost: i32) -> mpeg2ts::Result<()> {
        let units = split_annexb(au);
        if units.is_empty() {
            return Ok(());
        }
        let mut has_vcl = false;
        let mut is_key = false;
        let mut has_ps = false;
        let mut fresh_ps: Vec<u8> = Vec::new();
        for u in &units {
            match classify(self.codec(), u) {
                NalKind::Vcl { key } => {
                    has_vcl = true;
                    is_key |= key;
                }
                NalKind::ParamSet => {
                    has_ps = true;
                    fresh_ps.extend_from_slice(u);
                }
                NalKind::Other => {}
            }
        }
        // 参数集刷新缓存 (IDR 内嵌 SPS/PPS 也能自愈)。
        if !fresh_ps.is_empty() {
            self.ps_cache = fresh_ps;
        }
        // 纯参数集样本 (流头的 header 回调): 只缓存, 不输出帧。
        if !has_vcl {
            return Ok(());
        }
        let pts = self.video_stamp(frames_lost.max(0) as u64);
        let mut frame: Vec<u8> = Vec::with_capacity(au.len() + self.ps_cache.len());
        if is_key && !has_ps && !self.ps_cache.is_empty() {
            frame.extend_from_slice(&self.ps_cache);
        }
        frame.extend_from_slice(au);
        self.emit_pat_pmt()?;
        let adapt = AdaptationField {
            discontinuity_indicator: false,
            random_access_indicator: is_key,
            es_priority_indicator: false,
            pcr: Some(ClockReference::from(Timestamp::new(pts)?)),
            opcr: None,
            splice_countdown: None,
            transport_private_data: vec![],
            extension: None,
        };
        self.write_pes(self.video_pid, 2, 0xE0, pts, &frame, Some(adapt))?;
        // 每帧刷一次, 管道下游实时可见。
        self.out.flush()?;
        Ok(())
    }

    pub fn push_audio(&mut self, opus: &[u8]) -> mpeg2ts::Result<()> {
        if opus.is_empty() {
            // PLC 丢包: 时间线照样推进, 不输出。
            self.audio_skip();
            return Ok(());
        }
        let pts = self.audio_stamp();
        self.emit_pat_pmt()?;
        self.write_pes(self.audio_pid, 3, 0xC0, pts, opus, None)?;
        Ok(())
    }

    pub fn flush(&mut self) -> mpeg2ts::Result<()> {
        self.out.flush()?;
        Ok(())
    }

    fn codec(&self) -> VideoCodec {
        match self.video_stream_type {
            StreamType::H264 => VideoCodec::H264,
            _ => VideoCodec::H265,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 合成的 H264 AU: SPS + PPS + IDR (带 3/4 字节起始码混合)。
    fn sample_h264() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E]; // SPS (7)
        au.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x38, 0x80]); // PPS (8)
        au.extend_from_slice(&[0, 0, 1, 0x65, 0x88, 0x84, 0x21, 0xA0]); // IDR (5)
        au
    }

    fn sample_h265() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1, 0x42, 0x01, 0xAA]; // VPS (32)
        au.extend_from_slice(&[0, 0, 1, 0x44, 0x01, 0xBB]); // SPS (33)
        au.extend_from_slice(&[0, 0, 1, 0x46, 0x01, 0xCC]); // PPS (34)
        au.extend_from_slice(&[0, 0, 1, 0x26, 0x01, 0xDD]); // IDR_W_RADL (19)
        au
    }

    #[test]
    fn annexb_split_mixed_start_codes() {
        let au = sample_h264();
        let units = split_annexb(&au);
        assert_eq!(units.len(), 3);
        assert!(units[0].starts_with(&[0, 0, 0, 1]));
        assert!(units[1].starts_with(&[0, 0, 1]));
        // 类型字节正确。
        assert_eq!(units[0][4] & 0x1F, 7);
        assert_eq!(units[1][3] & 0x1F, 8);
        assert_eq!(units[2][3] & 0x1F, 5);
    }

    #[test]
    fn classify_h264() {
        let au = sample_h264();
        let units = split_annexb(&au);
        assert!(matches!(classify(VideoCodec::H264, units[0]), NalKind::ParamSet));
        assert!(matches!(classify(VideoCodec::H264, units[1]), NalKind::ParamSet));
        assert!(matches!(
            classify(VideoCodec::H264, units[2]),
            NalKind::Vcl { key: true }
        ));
        // 非 IDR P 帧。
        let p = [0u8, 0, 1, 0x41, 0x9A];
        assert!(matches!(
            classify(VideoCodec::H264, &p),
            NalKind::Vcl { key: false }
        ));
    }

    #[test]
    fn classify_h265() {
        let au = sample_h265();
        let units = split_annexb(&au);
        assert_eq!(units.len(), 4);
        assert!(matches!(classify(VideoCodec::H265, units[0]), NalKind::ParamSet));
        assert!(matches!(
            classify(VideoCodec::H265, units[3]),
            NalKind::Vcl { key: true }
        ));
        // TRAIL_N (0) 非关键帧。
        let t = [0u8, 0, 1, 0x02, 0x01];
        assert!(matches!(
            classify(VideoCodec::H265, &t),
            NalKind::Vcl { key: false }
        ));
    }

    #[test]
    fn headers_only_are_cached_not_emitted() {
        let mut mux = Mux::new(Vec::new(), VideoCodec::H264, 30).unwrap();
        // 纯 SPS+PPS: 只缓存, 不输出。
        let hdr = [0u8, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E, 0, 0, 1, 0x68, 0xCE];
        mux.push_video(&hdr, 0).unwrap();
        let out = mux.out.into_inner().unwrap();
        assert!(out.is_empty(), "headers-only sample must not emit");
    }

    #[test]
    fn mux_roundtrip_structure() {
        let mut mux = Mux::new(Vec::new(), VideoCodec::H264, 30).unwrap();
        mux.push_video(&sample_h264(), 0).unwrap();
        mux.push_audio(&[0xFC, 0x11, 0x22, 0x33]).unwrap();
        mux.push_video(&[0, 0, 1, 0x41, 0x9A, 0xBC], 0).unwrap(); // P 帧
        mux.flush().unwrap();
        let out = mux.out.into_inner().unwrap();

        assert!(!out.is_empty());
        assert_eq!(out.len() % TsPacket::SIZE, 0, "188B 对齐");
        // sync 字节。
        for (i, chunk) in out.chunks_exact(TsPacket::SIZE).enumerate() {
            assert_eq!(chunk[0], 0x47, "packet {i} sync");
        }
        // PAT (pid 0) 与 PMT (0x100) 存在。
        let pids: Vec<u16> = out
            .chunks_exact(TsPacket::SIZE)
            .map(|c| (((c[1] & 0x1F) as u16) << 8) | c[2] as u16)
            .collect();
        assert!(pids.contains(&0), "PAT missing");
        assert!(pids.contains(&0x100), "PMT missing");
        assert!(pids.contains(&0x101), "video missing");
        assert!(pids.contains(&0x102), "audio missing");
        // CC 单调 (每 PID)。
        use std::collections::HashMap;
        let mut last: HashMap<u16, u8> = HashMap::new();
        for c in out.chunks_exact(TsPacket::SIZE) {
            let pid = (((c[1] & 0x1F) as u16) << 8) | c[2] as u16;
            let cc = c[3] & 0x0F;
            if let Some(&prev) = last.get(&pid) {
                assert_eq!(cc, (prev + 1) & 0x0F, "CC break on pid {pid:#x}");
            }
            last.insert(pid, cc);
        }
        // payload_unit_start 只出现在 PES 起始包。
        let mut saw_video_start = false;
        for c in out.chunks_exact(TsPacket::SIZE) {
            let pid = (((c[1] & 0x1F) as u16) << 8) | c[2] as u16;
            if pid == 0x101 && (c[1] & 0x40) != 0 {
                saw_video_start = true;
            }
        }
        assert!(saw_video_start, "no video PES start");
    }

    #[test]
    /// 视频 PTS 按计数推进 (含 frames_lost 间隙), 与 wall-clock 无关。
    fn video_pts_spacing_counts_lost_frames() {
        let mut mux = Mux::new(Vec::new(), VideoCodec::H264, 30).unwrap();
        mux.push_video(&sample_h264(), 0).unwrap();
        mux.push_video(&[0, 0, 1, 0x41, 0x9A], 2).unwrap(); // 中间丢 2 帧
        mux.flush().unwrap();
        let out = mux.out.into_inner().unwrap();
        let ptss = find_video_pts(&out);
        assert_eq!(ptss.len(), 2);
        // 第二帧 PTS = 第一帧 + 3 帧间隔 (自己 + 丢的 2 帧)。
        assert_eq!(ptss[1] - ptss[0], 3 * 3000);
    }

    /// 从输出字节流里找视频 PES 的 PTS。
    fn find_video_pts(out: &[u8]) -> Vec<u64> {
        let pat = [0u8, 0, 1, 0xE0, 0, 0, 0x84, 0x80, 0x05];
        let mut v = Vec::new();
        for i in 0..out.len().saturating_sub(14) {
            if out[i..].starts_with(&pat) {
                let b = &out[i + 9..i + 14];
                let pts = (((b[0] >> 1) & 0x07) as u64) << 30
                    | (b[1] as u64) << 22
                    | (((b[2] >> 1) & 0x7F) as u64) << 15
                    | (b[3] as u64) << 7
                    | (((b[4] >> 1) & 0x7F) as u64);
                v.push(pts);
            }
        }
        v
    }

    #[test]
    fn empty_audio_skipped() {
        let mut mux = Mux::new(Vec::new(), VideoCodec::H264, 30).unwrap();
        mux.push_audio(&[]).unwrap();
        let out = mux.out.into_inner().unwrap();
        assert!(out.is_empty());
    }

}
