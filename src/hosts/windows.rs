//! Windows: 注册表 `HKCU\Software\Chiaki\Chiaki\{registered_hosts,manual_hosts}`。
//!
//! 条目 = 子键, 字段 = 子键下的值。QSettings QByteArray 编码实测为
//! `"@ByteArray(" + 原始字节 Latin-1 + ")"`, 可能是 REG_SZ, 也可能是
//! 包着 UTF-16 的 REG_BINARY; 注册表无数组 size, 直接枚举子键。

use super::{collect_hosts, manual_ip_map, ChiakiHost, SettingsEntry};
use winreg::enums::{HKEY_CURRENT_USER, REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_SZ};
use winreg::RegKey;

/// `"@ByteArray(...)"` 解码: 每个字符即一个字节 (Latin-1)。
fn decode_bytearray(s: &str) -> Option<Vec<u8>> {
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

/// REG_BINARY 解码: UTF-16 的 `"@ByteArray(...)"`, 否则裸字节兜底。
fn decode_binary(bytes: &[u8], expect_len: usize) -> Option<Vec<u8>> {
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

impl SettingsEntry for RegKey {
    fn get_str(&self, name: &str) -> Option<String> {
        match raw_value(self, name)? {
            RawVal::Str(s) => Some(s),
            _ => None,
        }
    }

    fn get_u32(&self, name: &str) -> Option<u32> {
        match raw_value(self, name)? {
            RawVal::Dword(n) => Some(n),
            _ => None,
        }
    }

    /// 字节值: SZ 走 @ByteArray 解析, BIN 走混合解析, 长度必须吻合。
    fn get_bytes(&self, name: &str, expect_len: usize) -> Option<Vec<u8>> {
        match raw_value(self, name)? {
            RawVal::Str(s) => decode_bytearray(&s),
            RawVal::Bin(b) => decode_binary(&b, expect_len),
            _ => None,
        }
        .filter(|v| v.len() == expect_len)
    }
}

pub(super) fn read_hosts() -> Result<Vec<ChiakiHost>, String> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let reg_base = hkcu
        .open_subkey("Software\\Chiaki\\Chiaki\\registered_hosts")
        .map_err(|e| format!("open registered_hosts: {e} (official chiaki-ng GUI registered hosts?)"))?;

    // manual_hosts 建 MAC->IP 表。
    let mut manual_entries = Vec::new();
    if let Ok(man) = hkcu.open_subkey("Software\\Chiaki\\Chiaki\\manual_hosts") {
        for name in man.enum_keys().filter_map(|r| r.ok()) {
            if let Ok(k) = man.open_subkey(&name) {
                manual_entries.push(k);
            }
        }
    }
    let ip_by_mac = manual_ip_map(manual_entries);

    let reg_entries: Vec<(String, RegKey)> = reg_base
        .enum_keys()
        .filter_map(|r| r.ok())
        .filter_map(|name| {
            reg_base
                .open_subkey(&name)
                .ok()
                .map(|k| (format!("registered_hosts\\{name}"), k))
        })
        .collect();
    Ok(collect_hosts(reg_entries, &ip_by_mac))
}
