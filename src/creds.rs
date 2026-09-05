//! 凭证解析: hex Key 解析、target 解析、wakeup credential 换算。

use libchiaki::ffi;

/// 32 hex 字符 -> 16 字节。
pub fn parse_hex16(s: &str) -> Result<[u8; 16], String> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() != 32 {
        return Err(format!("need 32 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("invalid hex at byte {i}"))?;
    }
    Ok(out)
}


/// 解析 PSN account id: 只接受 base64, 8 字节 (与 chiaki-ng 一致)。
pub fn parse_account_id(s: &str) -> Result<[u8; 8], String> {
    let raw: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let trimmed = raw.trim_end_matches('=');
    if trimmed.len() != 11 && trimmed.len() != 12 {
        return Err(format!(
            "invalid account id: need 8-byte base64, got '{}'",
            trimmed.len()
        ));
    }
    let bytes = base64_decode(trimmed)?;
    if bytes.len() != 8 {
        return Err(format!("base64 decoded to {} bytes, expected 8", bytes.len()));
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// 标准 base64 解码 (不打散原始字节)。
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut value = [0u8; 256];
    for (i, &c) in table.iter().enumerate() {
        value[c as usize] = i as u8;
    }
    let chars: Vec<u8> = s.bytes().collect();
    let mut out = Vec::with_capacity(chars.len() / 4 * 3);
    let mut i = 0;
    while i < chars.len() {
        let c0 = value[chars[i] as usize];
        let c1 = if i + 1 < chars.len() {
            value[chars[i + 1] as usize]
        } else {
            return Err("invalid base64 length".into());
        };
        let mut c2 = None;
        let mut c3 = None;
        if i + 2 < chars.len() && chars[i + 2] != b'=' {
            c2 = Some(value[chars[i + 2] as usize]);
        }
        if i + 3 < chars.len() && chars[i + 3] != b'=' {
            c3 = Some(value[chars[i + 3] as usize]);
        }
        out.push((c0 << 2) | (c1 >> 4));
        if let Some(c2) = c2 {
            out.push(((c1 & 0x0f) << 4) | (c2 >> 2));
        }
        if let (Some(c2), Some(c3)) = (c2, c3) {
            out.push(((c2 & 0x03) << 6) | c3);
        }
        i += 4;
    }
    Ok(out)
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 把 16 字节 regist_key 数组的文本部分 (到首个 NUL 前) 还原为字符串。
///
/// chiaki-ng 的 `rp_regist_key` 存的是可打印文本 (如 "deadbeef")
/// 加 NUL 填充到 16 字节; 本函数提取该文本供 CLI 显示 / 传参。
pub fn regist_key_text(bytes: &[u8; 16]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 把命令行给定的 regist_key 文本 pad NUL 到 16 字节 (与 chiaki-ng 一致)。
pub fn expand_regist_key_text(text: &str) -> Result<[u8; 16], String> {
    let b = text.as_bytes();
    if b.len() > 16 {
        return Err(format!("regist key too long: {} bytes (max 16)", b.len()));
    }
    let mut out = [0u8; 16];
    out[..b.len()].copy_from_slice(b);
    Ok(out)
}

/// 解析注册目标版本。与 chiaki-ng GUI (RegistDialog) 的选项一致:
///   编号 | target  | 原版说明                    | 别名
///   1    | 800     | PS4 Firmware <  7.0        | ps4-1
///   2    | 900     | PS4 Firmware >= 7.0, < 8.0 | ps4-2
///   3    | 1000    | PS4 Firmware >= 8.0        | ps4-3
///   4    | 1000100 | PS5                        | ps5
pub fn parse_target(s: Option<&str>, ps5: bool) -> Result<ffi::ChiakiTarget, String> {
    use ffi::ChiakiTarget::*;
    let Some(s) = s else {
        return Ok(if ps5 {
            CHIAKI_TARGET_PS5_1
        } else {
            CHIAKI_TARGET_PS4_10
        });
    };
    let t = s.trim().to_lowercase();
    match t.as_str() {
        "1" | "ps4-1" => Ok(CHIAKI_TARGET_PS4_8),
        "2" | "ps4-2" => Ok(CHIAKI_TARGET_PS4_9),
        "3" | "ps4-3" | "ps4" => Ok(CHIAKI_TARGET_PS4_10),
        "4" | "ps5" | "ps5-1" => Ok(CHIAKI_TARGET_PS5_1),
        _ => Err(
            "unknown target '{s}'. options:\n  \
             1: PS4 Firmware <  7.0        (PS4-1)\n  \
             2: PS4 Firmware >= 7.0, < 8.0 (PS4-2)\n  \
             3: PS4 Firmware >= 8.0        (PS4-3)\n  \
             4: PS5                        (PS5)"
                .to_string(),
        ),
    }
}

/// wakeup credential: regist_key(16B) 按 chiaki-ng GUI 同款逻辑换算 u64
/// (bytes -> UTF-8 字符串 -> trim NUL -> 按 16 进制解析)。
pub fn wakeup_credential(regist_key: &[u8; 16]) -> Result<u64, String> {
    let s = String::from_utf8_lossy(regist_key);
    let s = s.trim_matches('\0').trim();
    if s.is_empty() {
        return Err("regist_key is empty".to_string());
    }
    u64::from_str_radix(s, 16).map_err(|_| format!("cannot parse credential from '{s}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_base64() {
        let v = parse_account_id("NDJvGJAjYVQ=").unwrap();
        assert_eq!(v, [0x34, 0x32, 0x6f, 0x18, 0x90, 0x23, 0x61, 0x54]);
    }

    #[test]
    fn account_id_goldhen_all_ff() {
        let v = parse_account_id("fffffffffff=").unwrap();
        assert_eq!(v, [0x7d, 0xf7, 0xdf, 0x7d, 0xf7, 0xdf, 0x7d, 0xf7]);
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;
    use libchiaki::ffi::ChiakiTarget::{self, *};

    fn parse(s: &str) -> ChiakiTarget { parse_target(Some(s), false).unwrap() }

    #[test]
    fn target_numbers() {
        assert_eq!(parse("1"), CHIAKI_TARGET_PS4_8);
        assert_eq!(parse("2"), CHIAKI_TARGET_PS4_9);
        assert_eq!(parse("3"), CHIAKI_TARGET_PS4_10);
        assert_eq!(parse("4"), CHIAKI_TARGET_PS5_1);
    }
    #[test]
    fn target_aliases() {
        assert_eq!(parse("ps4-1"), CHIAKI_TARGET_PS4_8);
        assert_eq!(parse("ps4-2"), CHIAKI_TARGET_PS4_9);
        assert_eq!(parse("ps4-3"), CHIAKI_TARGET_PS4_10);
        assert_eq!(parse("ps4"), CHIAKI_TARGET_PS4_10);
        assert_eq!(parse("ps5"), CHIAKI_TARGET_PS5_1);
        assert_eq!(parse("ps5-1"), CHIAKI_TARGET_PS5_1);
    }
}
