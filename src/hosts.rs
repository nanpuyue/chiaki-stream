//! 官方 chiaki-ng 已注册主机读取 (Windows 注册表)。
//!
//! 存储位置: `HKCU\Software\Chiaki\Chiaki\{registered_hosts,manual_hosts}`。
//! QSettings QByteArray 编码实测为 `"@ByteArray(" + 原始字节 Latin-1 + ")"`,
//! 可能是 REG_SZ, 也可能是包着 UTF-16 的 REG_BINARY。
//! IP 通过 `manual_hosts[].registered_mac == registered_hosts[].server_mac`
//! 关联出来。

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

/// `"@ByteArray(...)"` 解码: 每个字符即一个字节 (Latin-1)。
pub fn decode_bytearray(s: &str) -> Option<Vec<u8>> {
    let inner = s.strip_prefix("@ByteArray(")?;
    // 只剥末尾一个 ')' (数据里若有 ')' 字节则保留)。
    let inner = inner.strip_suffix(')')?;
    inner
        .chars()
        .map(|c| {
            let u = c as u32;
            if u < 256 { Some(u as u8) } else { None }
        })
        .collect()
}

/// REG_BINARY 解码: UTF-16 的 `"@ByteArray(...)"`, 否则裸字节兜底
/// (长度必须吻合才收)。
pub fn decode_binary(bytes: &[u8], expect_len: usize) -> Option<Vec<u8>> {
    if bytes.len() >= 2 && bytes.len() % 2 == 0 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        if let Ok(s) = String::from_utf16(&units) {
            if let Some(v) = decode_bytearray(&s) {
                return Some(v);
            }
        }
    }
    if bytes.len() == expect_len {
        return Some(bytes.to_vec());
    }
    None
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
mod win {
    use super::*;
    use winreg::enums::{HKEY_CURRENT_USER, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_SZ};
    use winreg::RegKey;

    /// 自有化后的注册表值 (winreg 的 RegValue 借用 key, 不能返回)。
    enum RawVal {
        Dword(u32),
        Str(String),
        Bin(Vec<u8>),
    }

    fn raw_value(key: &RegKey, name: &str) -> Option<RawVal> {
        let v = key.get_raw_value(name).ok()?;
        match v.vtype {
            REG_DWORD if v.bytes.len() == 4 => Some(RawVal::Dword(u32::from_le_bytes([
                v.bytes[0], v.bytes[1], v.bytes[2], v.bytes[3],
            ]))),
            REG_SZ | REG_EXPAND_SZ => {
                let units: Vec<u16> = v.bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                String::from_utf16(&units)
                    .ok()
                    .map(|s| RawVal::Str(s.trim_end_matches('\0').to_string()))
            }
            REG_BINARY => Some(RawVal::Bin(v.bytes.to_vec())),
            _ => None,
        }
    }

    fn str_value(key: &RegKey, name: &str) -> Option<String> {
        match raw_value(key, name)? {
            RawVal::Str(s) => Some(s),
            _ => None,
        }
    }

    fn u32_value(key: &RegKey, name: &str) -> Option<u32> {
        match raw_value(key, name)? {
            RawVal::Dword(n) => Some(n),
            _ => None,
        }
    }

    /// 字节值: SZ 走 @ByteArray 解析, BIN 走混合解析, 长度必须吻合。
    fn bytes_value(key: &RegKey, name: &str, expect_len: usize) -> Option<Vec<u8>> {
        let out = match raw_value(key, name)? {
            RawVal::Str(s) => decode_bytearray(&s)?,
            RawVal::Bin(b) => decode_binary(&b, expect_len)?,
            _ => return None,
        };
        (out.len() == expect_len).then_some(out)
    }

    fn read_host(key: &RegKey) -> Option<ChiakiHost> {
        let target = u32_value(key, "target")?;
        let regist_key: [u8; 16] = bytes_value(key, "rp_regist_key", 16)?.try_into().ok()?;
        let morning: [u8; 16] = bytes_value(key, "rp_key", 16)?.try_into().ok()?;
        let mac = bytes_value(key, "server_mac", 6).and_then(|v| v.try_into().ok());
        Some(ChiakiHost {
            nickname: str_value(key, "server_nickname").unwrap_or_else(|| "?".to_string()),
            host_ip: None,
            mac,
            target,
            ps5: target >= 1000000,
            regist_key,
            morning,
        })
    }

    pub(super) fn read_hosts() -> Result<Vec<ChiakiHost>, String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let reg_base = hkcu
            .open_subkey("Software\\Chiaki\\Chiaki\\registered_hosts")
            .map_err(|e| format!("open registered_hosts: {e} (official chiaki-ng GUI registered hosts?)"))?;

        // manual_hosts 建 MAC->IP 表。
        let mut ip_by_mac: HashMap<[u8; 6], String> = HashMap::new();
        if let Ok(man) = hkcu.open_subkey("Software\\Chiaki\\Chiaki\\manual_hosts") {
            for name in man.enum_keys().filter_map(|r| r.ok()) {
                let Ok(k) = man.open_subkey(&name) else {
                    continue;
                };
                let (Some(mac), Some(ip)) =
                    (bytes_value(&k, "registered_mac", 6), str_value(&k, "host"))
                else {
                    continue;
                };
                if ip.is_empty() {
                    continue;
                }
                if let Ok(mac) = <Vec<u8> as TryInto<[u8; 6]>>::try_into(mac) {
                    ip_by_mac.insert(mac, ip);
                }
            }
        }

        let mut out = Vec::new();
        for name in reg_base.enum_keys().filter_map(|r| r.ok()) {
            let Ok(k) = reg_base.open_subkey(&name) else {
                continue;
            };
            let Some(mut h) = read_host(&k) else {
                eprintln!("warn: skip broken registered_hosts\\{name}");
                continue;
            };
            if let Some(mac) = h.mac {
                h.host_ip = ip_by_mac.get(&mac).cloned();
            }
            out.push(h);
        }
        Ok(out)
    }

    #[allow(dead_code)]
    fn _use_consts() {
        // 保留类型引用, 防止改版时漏掉。
        let _ = (REG_SZ, REG_BINARY, REG_DWORD, REG_EXPAND_SZ);
    }
}

/// 读取官方 chiaki-ng 已注册主机 (仅 Windows)。
#[cfg(windows)]
pub fn read_chiaki_hosts() -> Result<Vec<ChiakiHost>, String> {
    win::read_hosts()
}

/// 非 Windows: 不支持。
#[cfg(not(windows))]
pub fn read_chiaki_hosts() -> Result<Vec<ChiakiHost>, String> {
    Err("reading chiaki hosts is only supported on Windows".to_string())
}
