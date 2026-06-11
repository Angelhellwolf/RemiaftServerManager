use std::fs;
use std::io;
use std::path::Path;

use encoding_rs::GBK;

pub fn read_console_text(path: impl AsRef<Path>) -> io::Result<String> {
    fs::read(path).map(|bytes| decode_console_bytes(&bytes))
}

pub fn decode_console_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Ok(text) = std::str::from_utf8(bytes) {
        return repair_gbk_mojibake(text).unwrap_or_else(|| text.to_string());
    }

    let (decoded, _, had_errors) = GBK.decode(bytes);
    if !had_errors {
        return decoded.into_owned();
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn repair_gbk_mojibake(text: &str) -> Option<String> {
    if mojibake_score(text) < 3 {
        return None;
    }

    let mut bytes = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let code = ch as u32;
        if code <= 0xff {
            bytes.push(code as u8);
        } else {
            return None;
        }
    }

    let (decoded, _, had_errors) = GBK.decode(&bytes);
    if had_errors {
        return None;
    }

    let decoded = decoded.into_owned();
    if cjk_score(&decoded) > cjk_score(text) && mojibake_score(&decoded) < mojibake_score(text) {
        Some(decoded)
    } else {
        None
    }
}

fn cjk_score(text: &str) -> usize {
    text.chars()
        .filter(|ch| matches!(*ch as u32, 0x4e00..=0x9fff))
        .count()
}

fn mojibake_score(text: &str) -> usize {
    text.chars()
        .filter(|ch| {
            matches!(
                *ch,
                'Î' | 'ï'
                    | 'Æ'
                    | '·'
                    | 'µ'
                    | 'Ä'
                    | '²'
                    | 'Û'
                    | 'Þ'
                    | 'Ð'
                    | '§'
                    | '£'
                    | '¬'
                    | 'Õ'
                    | 'û'
            )
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_raw_gbk_console_bytes() {
        assert_eq!(decode_console_bytes(&[0xce, 0xef, 0xc6, 0xb7]), "物品");
    }

    #[test]
    fn repairs_utf8_text_containing_gbk_mojibake() {
        assert_eq!(
            decode_console_bytes("ÎïÆ· red µÄ²ÛÎ»ÎÞÐ§".as_bytes()),
            "物品 red 的槽位无效"
        );
    }
}
