//! macOS: 读 QSettings 原生 plist `~/Library/Preferences/com.chiaki.Chiaki.plist`。
//!
//! QSettings 数组在原生格式里落成扁平点分键 ("/" 映射为 "."):
//! `registered_hosts.size` 计数, 条目为 `registered_hosts.<N>.<字段>`,
//! N 从 1 起 (与 INI 格式同约定; chiaki-ng 源码里 setArrayIndex 是 0 基,
//! 落盘后整体 +1)。条目字段访问经 [`PlistEntry`] 对接到通用逻辑。
//!
//! 注意: 直接读盘面文件; chiaki-ng 运行中的改动经 cfprefsd 回写有短暂延迟,
//! 一般退出/保存时已落盘。

use super::{collect_hosts, manual_ip_map, ChiakiHost, SettingsEntry};
use plist::{Dictionary, Value};
use std::path::PathBuf;

/// QSettings(org="Chiaki", app="Chiaki") 在 macOS 的原生位置。
fn settings_path() -> PathBuf {
    let mut path = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    path.push("Library/Preferences/com.chiaki.Chiaki.plist");
    path
}

/// plist 扁平键条目: 字段名拼成 `{prefix}.{name}` 再查字典。
struct PlistEntry<'a> {
    dict: &'a Dictionary,
    prefix: String,
}

/// plist 整数值: 有符号/无符号都收, 超出 u32 视为缺失。
fn value_u32(v: &Value) -> Option<u32> {
    let n = v
        .as_unsigned_integer()
        .or_else(|| v.as_signed_integer().map(|n| n as u64))?;
    u32::try_from(n).ok()
}

impl SettingsEntry for PlistEntry<'_> {
    fn get_str(&self, name: &str) -> Option<String> {
        self.dict
            .get(&format!("{}.{}", self.prefix, name))?
            .as_string()
            .map(str::to_string)
    }

    fn get_u32(&self, name: &str) -> Option<u32> {
        value_u32(self.dict.get(&format!("{}.{}", self.prefix, name))?)
    }

    fn get_bytes(&self, name: &str, expect_len: usize) -> Option<Vec<u8>> {
        self.dict
            .get(&format!("{}.{}", self.prefix, name))?
            .as_data()
            .filter(|b| b.len() == expect_len)
            .map(<[u8]>::to_vec)
    }
}

fn entry<'a>(dict: &'a Dictionary, array: &str, i: u32) -> PlistEntry<'a> {
    PlistEntry {
        dict,
        prefix: format!("{array}.{i}"),
    }
}

fn get_u32(dict: &Dictionary, key: &str) -> Option<u32> {
    value_u32(dict.get(key)?)
}

fn parse_hosts(dict: &Dictionary) -> Result<Vec<ChiakiHost>, String> {
    // manual_hosts 建 MAC->IP 表 (同样 1 基; 键残留的旧条目不读)。
    let man_count = get_u32(dict, "manual_hosts.size").unwrap_or(0);
    let ip_by_mac = manual_ip_map((1..=man_count).map(|i| entry(dict, "manual_hosts", i)));

    let count = get_u32(dict, "registered_hosts.size").ok_or(
        "no registered_hosts.size in chiaki-ng settings (official chiaki-ng GUI registered hosts?)",
    )?;
    let reg_entries = (1..=count)
        .map(|i| (format!("registered_hosts.{i}"), entry(dict, "registered_hosts", i)))
        .collect::<Vec<_>>();
    Ok(collect_hosts(reg_entries, &ip_by_mac))
}

pub(super) fn read_hosts() -> Result<Vec<ChiakiHost>, String> {
    let path = settings_path();
    let value = Value::from_file(&path).map_err(|e| {
        format!(
            "open {}: {e} (official chiaki-ng GUI registered hosts?)",
            path.display()
        )
    })?;
    let dict = value
        .into_dictionary()
        .ok_or_else(|| format!("{}: not a plist dictionary", path.display()))?;
    parse_hosts(&dict)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试合成数据: MAC 用 RFC 7042 文档段, IP 用 RFC 5737 TEST-NET,
    /// 与任何真实 chiaki-ng 数据无关。
    const TEST_MAC: [u8; 6] = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
    const TEST_IP: &str = "192.0.2.10";

    /// 按真实 plist 的数据类型构造 (int / data / string)。
    fn data(b: Vec<u8>) -> Value {
        Value::Data(b)
    }

    fn fixture() -> Dictionary {
        let mut d = Dictionary::new();
        d.insert("registered_hosts.size".into(), 1u64.into());
        d.insert("registered_hosts.1.target".into(), 1000100u64.into());
        d.insert("registered_hosts.1.server_nickname".into(), "PS5".into());
        d.insert(
            "registered_hosts.1.server_mac".into(),
            data(TEST_MAC.to_vec()),
        );
        d.insert(
            "registered_hosts.1.rp_regist_key".into(),
            {
                let mut k = vec![0u8; 16];
                k[..8].copy_from_slice(b"deadbeef");
                data(k)
            },
        );
        d.insert(
            "registered_hosts.1.rp_key".into(),
            data(b"0123456789abcdef".to_vec()),
        );
        // 键残留: size=1 时 .2 不应被读到。
        d.insert("registered_hosts.2.target".into(), 1000u64.into());
        d.insert("registered_hosts.2.rp_regist_key".into(), data(vec![0u8; 16]));
        d.insert("registered_hosts.2.rp_key".into(), data(vec![0u8; 16]));
        d.insert("manual_hosts.size".into(), 1u64.into());
        d.insert("manual_hosts.1.host".into(), TEST_IP.into());
        d.insert(
            "manual_hosts.1.registered_mac".into(),
            data(TEST_MAC.to_vec()),
        );
        d
    }

    #[test]
    fn parse_respects_size_and_links_ip() {
        let hosts = parse_hosts(&fixture()).unwrap();
        assert_eq!(hosts.len(), 1, "残留的 .2 条目不应被读出");
        let h = &hosts[0];
        assert_eq!(h.nickname, "PS5");
        assert_eq!(h.host_ip.as_deref(), Some(TEST_IP));
        assert_eq!(h.mac, Some(TEST_MAC));
        assert_eq!(h.target, 1000100);
        assert!(h.ps5);
        assert_eq!(&h.regist_key[..8], b"deadbeef");
    }

    #[test]
    fn missing_size_is_error() {
        assert!(parse_hosts(&Dictionary::new()).is_err());
    }
}
