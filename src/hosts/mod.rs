//! 官方 chiaki-ng 已配对主机读取。
//!
//! chiaki-ng 用 QSettings(org="Chiaki", app="Chiaki") 存 `registered_hosts` /
//! `manual_hosts` 两个数组, 各平台的原生落地:
//! - Windows: 注册表 `HKCU\Software\Chiaki\Chiaki\{registered_hosts,manual_hosts}` (windows.rs)。
//! - macOS:   plist `~/Library/Preferences/com.chiaki.Chiaki.plist` (macos.rs),
//!   数组为扁平键 `registered_hosts.<N>.<字段>` (N 从 1 起, 另有 `<前缀>.size` 计数)。
//!
//! 两平台的字段名一致 (target / rp_regist_key / rp_key / server_mac /
//! server_nickname / registered_mac / host), 差异只在"怎么把一个条目的某个
//! 字段读出来", 由 [`SettingsEntry`] 抽象; 条目组装 ([`read_host`])、
//! manual_hosts 的 MAC->IP 关联 ([`manual_ip_map`])、枚举 + 坏条目跳过
//! ([`collect_hosts`]) 都是平台无关的通用逻辑。
//! IP 通过 `manual_hosts[].registered_mac == registered_hosts[].server_mac` 关联出来。

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct ChiakiHost {
    pub nickname: String,
    pub host_ip: Option<String>,
    pub mac: Option<[u8; 6]>,
    pub target: u32,
    pub ps5: bool,
    pub regist_key: [u8; 16],
    pub morning: [u8; 16],
}

/// target >= 该值视为 PS5 (PS5 的 target 为 1000100, PS4 最大 1000)。
const PS5_TARGET_MIN: u32 = 1_000_000;

/// 平台存储的单条主机记录的字段访问 (windows=注册表子键, macos=plist 键前缀)。
/// 长度/类型不吻合的字段一律按缺失处理, 与 chiaki-ng 读取口径一致。
trait SettingsEntry {
    fn get_str(&self, name: &str) -> Option<String>;
    fn get_u32(&self, name: &str) -> Option<u32>;
    /// 定长字节字段, 长度不吻合视为缺失。
    fn get_bytes(&self, name: &str, expect_len: usize) -> Option<Vec<u8>>;
}

/// 从条目字段组装主机; 缺 target / rp_regist_key / rp_key 任一关键字段则 None。
fn read_host(e: &impl SettingsEntry) -> Option<ChiakiHost> {
    let target = e.get_u32("target")?;
    let regist_key: [u8; 16] = e.get_bytes("rp_regist_key", 16)?.try_into().ok()?;
    let morning: [u8; 16] = e.get_bytes("rp_key", 16)?.try_into().ok()?;
    let mac = e.get_bytes("server_mac", 6).and_then(|v| v.try_into().ok());
    Some(ChiakiHost {
        nickname: e
            .get_str("server_nickname")
            .unwrap_or_else(|| "?".to_string()),
        host_ip: None,
        mac,
        target,
        ps5: target >= PS5_TARGET_MIN,
        regist_key,
        morning,
    })
}

/// manual_hosts 条目建 MAC->IP 表; 空 IP / 坏 MAC 跳过, 同 MAC 取先出现的。
fn manual_ip_map<S: SettingsEntry>(
    entries: impl IntoIterator<Item = S>,
) -> HashMap<[u8; 6], String> {
    let mut ip_by_mac = HashMap::new();
    for e in entries {
        let (Some(mac), Some(ip)) = (
            e.get_bytes("registered_mac", 6)
                .and_then(|v| v.try_into().ok()),
            e.get_str("host"),
        ) else {
            continue;
        };
        if !ip.is_empty() {
            ip_by_mac.entry(mac).or_insert(ip);
        }
    }
    ip_by_mac
}

/// registered_hosts 条目 -> 主机列表; 坏条目打警告跳过 (label 用于指明是哪条),
/// 并按 MAC 关联 manual_hosts 里的 IP。
fn collect_hosts<S: SettingsEntry>(
    entries: impl IntoIterator<Item = (String, S)>,
    ip_by_mac: &HashMap<[u8; 6], String>,
) -> Vec<ChiakiHost> {
    let mut out = Vec::new();
    for (label, e) in entries {
        let Some(mut h) = read_host(&e) else {
            eprintln!("warn: skip broken {label}");
            continue;
        };
        if let Some(mac) = h.mac {
            h.host_ip = ip_by_mac.get(&mac).cloned();
        }
        out.push(h);
    }
    out
}

#[cfg(test)]
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let h: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if h.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// 按昵称 (大小写不敏感) / IP / MAC 查找。
pub fn find_host<'a>(hosts: &'a [ChiakiHost], query: &str) -> Result<&'a ChiakiHost, String> {
    let q = query.trim();
    if let Some(h) = hosts.iter().find(|h| h.nickname.eq_ignore_ascii_case(q)) {
        return Ok(h);
    }
    if let Some(h) = hosts.iter().find(|h| h.host_ip.as_deref() == Some(q)) {
        return Ok(h);
    }
    if let Some(m) = parse_mac(q) {
        if let Some(h) = hosts.iter().find(|h| h.mac == Some(m)) {
            return Ok(h);
        }
    }
    let names: Vec<&str> = hosts.iter().map(|h| h.nickname.as_str()).collect();
    Err(format!(
        "no chiaki-ng host matches '{q}'; available: {}",
        names.join(", ")
    ))
}

#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
mod macos;

/// 读取官方 chiaki-ng 已注册主机 (仅 Windows)。
#[cfg(windows)]
pub fn read_chiaki_hosts() -> Result<Vec<ChiakiHost>, String> {
    windows::read_hosts()
}

/// 读取官方 chiaki-ng 已注册主机 (仅 macOS)。
#[cfg(target_os = "macos")]
pub fn read_chiaki_hosts() -> Result<Vec<ChiakiHost>, String> {
    macos::read_hosts()
}

/// 其他平台: 不支持。
#[cfg(not(any(windows, target_os = "macos")))]
pub fn read_chiaki_hosts() -> Result<Vec<ChiakiHost>, String> {
    Err("reading chiaki hosts is only supported on Windows and macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// 内存版条目, 验证与平台无关的通用逻辑。
    #[derive(Default)]
    struct Fake(BTreeMap<&'static str, Val>);

    #[derive(Default)]
    enum Val {
        #[default]
        None,
        Str(String),
        U32(u32),
        Bytes(Vec<u8>),
    }

    impl SettingsEntry for Fake {
        fn get_str(&self, name: &str) -> Option<String> {
            match self.0.get(name)? {
                Val::Str(s) => Some(s.clone()),
                _ => None,
            }
        }
        fn get_u32(&self, name: &str) -> Option<u32> {
            match self.0.get(name)? {
                Val::U32(n) => Some(*n),
                _ => None,
            }
        }
        fn get_bytes(&self, name: &str, expect_len: usize) -> Option<Vec<u8>> {
            match self.0.get(name)? {
                Val::Bytes(b) if b.len() == expect_len => Some(b.clone()),
                _ => None,
            }
        }
    }

    /// 测试合成数据: MAC 用 RFC 7042 文档段, IP 用 RFC 5737 TEST-NET,
    /// 与任何真实 chiaki-ng 数据无关。
    const TEST_MAC: [u8; 6] = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
    const TEST_IP: &str = "192.0.2.10";

    /// 完整可用的 PS5 主机条目。
    fn ps5_entry() -> Fake {
        Fake(BTreeMap::from([
            ("target", Val::U32(1000100)),
            ("rp_regist_key", Val::Bytes({
                let mut k = vec![0u8; 16];
                k[..8].copy_from_slice(b"deadbeef");
                k
            })),
            ("rp_key", Val::Bytes(b"0123456789abcdef".to_vec())),
            ("server_mac", Val::Bytes(TEST_MAC.to_vec())),
            ("server_nickname", Val::Str("PS5".into())),
        ]))
    }

    #[test]
    fn read_host_requires_key_fields() {
        assert!(read_host(&Fake::default()).is_none());
        // 缺 rp_key 不算完整条目。
        let mut e = ps5_entry();
        e.0.remove("rp_key");
        assert!(read_host(&e).is_none());
        // 长度不吻合按缺失处理。
        let mut e = ps5_entry();
        e.0.insert("rp_key", Val::Bytes(vec![0u8; 15]));
        assert!(read_host(&e).is_none());
    }

    #[test]
    fn read_host_optional_fields() {
        // server_mac / server_nickname 可缺, ps5 按 target 推导。
        let mut e = ps5_entry();
        e.0.remove("server_mac");
        e.0.remove("server_nickname");
        let h = read_host(&e).unwrap();
        assert_eq!(h.nickname, "?");
        assert_eq!(h.mac, None);
        assert!(h.ps5);
        // PS4 (target=1000)。
        let mut e = ps5_entry();
        e.0.insert("target", Val::U32(1000));
        assert!(!read_host(&e).unwrap().ps5);
    }

    #[test]
    fn collect_links_ip_and_skips_broken() {
        let ip_by_mac = manual_ip_map([
            ps5_entry(), // manual 条目里 target 等字段多余, 只看 registered_mac/host
        ]);
        assert!(ip_by_mac.is_empty());

        let ip_by_mac = manual_ip_map([Fake(BTreeMap::from([
            ("registered_mac", Val::Bytes(TEST_MAC.to_vec())),
            ("host", Val::Str(TEST_IP.into())),
        ]))]);
        let mut e = ps5_entry();
        e.0.remove("rp_key"); // 一条坏条目
        let hosts = collect_hosts([("registered_hosts.1".into(), e)], &ip_by_mac);
        assert!(hosts.is_empty());

        let hosts = collect_hosts([("registered_hosts.1".into(), ps5_entry())], &ip_by_mac);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host_ip.as_deref(), Some(TEST_IP));
    }

    #[test]
    fn manual_map_ignores_empty_ip_and_takes_first() {
        let ip_by_mac = manual_ip_map([
            Fake(BTreeMap::from([
                ("registered_mac", Val::Bytes(TEST_MAC.to_vec())),
                ("host", Val::Str("".into())),
            ])),
            Fake(BTreeMap::from([
                ("registered_mac", Val::Bytes(TEST_MAC.to_vec())),
                ("host", Val::Str("192.0.2.1".into())),
            ])),
            Fake(BTreeMap::from([
                ("registered_mac", Val::Bytes(TEST_MAC.to_vec())),
                ("host", Val::Str("192.0.2.2".into())),
            ])),
        ]);
        assert_eq!(ip_by_mac.get(&TEST_MAC).map(String::as_str), Some("192.0.2.1"));
    }

    #[test]
    fn find_host_by_nickname_ip_mac() {
        let ip_by_mac = manual_ip_map([Fake(BTreeMap::from([
            ("registered_mac", Val::Bytes(TEST_MAC.to_vec())),
            ("host", Val::Str(TEST_IP.into())),
        ]))]);
        let hosts = collect_hosts([("registered_hosts.1".into(), ps5_entry())], &ip_by_mac);
        assert_eq!(find_host(&hosts, "ps5").unwrap().target, 1000100);
        assert_eq!(find_host(&hosts, TEST_IP).unwrap().target, 1000100);
        assert_eq!(
            find_host(&hosts, &format_mac(&TEST_MAC)).unwrap().target,
            1000100
        );
        assert!(find_host(&hosts, "nope").is_err());
    }
}
