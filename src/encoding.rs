//! Console output decoding.
//!
//! Server processes write their console output in whatever encoding the JVM
//! (or any other runtime) picked: UTF-8 on most modern setups, but GBK,
//! Big5, Shift_JIS, EUC-KR, or a windows-125x code page on legacy-locale
//! systems. Decoding those bytes as lossy UTF-8 fills the UI with
//! replacement characters, so every place that turns log bytes into text
//! goes through this module instead: strict UTF-8 first, then automatic
//! charset detection (chardetng, the detector Firefox uses), then lossy
//! UTF-8 as the last resort.

/// Once this much output accumulates without a newline, a legacy-encoded
/// stream is decoded anyway so interactive prompts are not held back forever.
const MAX_PENDING_BYTES: usize = 8 * 1024;

/// Decodes a complete buffer of console bytes for display.
pub fn decode_console_bytes(buf: &[u8]) -> String {
    let complete = trim_partial_utf8_suffix(buf);
    if let Ok(text) = std::str::from_utf8(complete) {
        return text.to_string();
    }
    let mut detector = chardetng::EncodingDetector::new();
    detector.feed(buf, true);
    let encoding = detector.guess(None, true);
    let (text, _, had_errors) = encoding.decode(buf);
    if !had_errors {
        return text.into_owned();
    }
    String::from_utf8_lossy(buf).into_owned()
}

/// Drops an incomplete multi-byte UTF-8 sequence from the end of `buf`. The
/// producer may be mid-write when a tail is read; one truncated character
/// must not force the whole buffer into the legacy-encoding fallback or
/// leave a stray replacement character on the last line.
fn trim_partial_utf8_suffix(buf: &[u8]) -> &[u8] {
    match std::str::from_utf8(buf) {
        Ok(_) => buf,
        Err(err) if err.error_len().is_none() => &buf[..err.valid_up_to()],
        Err(_) => buf,
    }
}

/// Incremental decoder for live console streams, where a read chunk can end
/// in the middle of a multi-byte character. UTF-8 input is emitted
/// immediately (holding back only an incomplete trailing character), which
/// keeps interactive prompts responsive; once the bytes cannot be UTF-8 the
/// stream buffers up to the next newline so charset detection sees whole
/// lines instead of arbitrary chunk boundaries.
#[derive(Default)]
pub struct StreamDecoder {
    pending: Vec<u8>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds `bytes` in and returns whatever is ready to display.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => {
                let ready = std::mem::take(&mut self.pending);
                String::from_utf8(ready).expect("validated above")
            }
            Err(err) if err.error_len().is_none() => {
                let ready: Vec<u8> = self.pending.drain(..err.valid_up_to()).collect();
                String::from_utf8(ready).expect("validated above")
            }
            Err(_) => {
                let split = match self.pending.iter().rposition(|byte| *byte == b'\n') {
                    Some(newline) => newline + 1,
                    None if self.pending.len() >= MAX_PENDING_BYTES => self.pending.len(),
                    None => return String::new(),
                };
                let ready: Vec<u8> = self.pending.drain(..split).collect();
                decode_console_bytes(&ready)
            }
        }
    }

    /// Decodes anything still buffered; call when the stream ends.
    pub fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let rest = std::mem::take(&mut self.pending);
        decode_console_bytes(&rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_valid_utf8_intact() {
        let text = "[11:31:38 WARN]: 服务器日志 with ANSI \u{1b}[31mred\u{1b}[0m";
        assert_eq!(decode_console_bytes(text.as_bytes()), text);
    }

    #[test]
    fn drops_partial_utf8_character_at_end() {
        let mut bytes = "日志".as_bytes().to_vec();
        bytes.pop();
        assert_eq!(decode_console_bytes(&bytes), "日");
    }

    #[test]
    fn never_panics_on_arbitrary_bytes() {
        let bytes = [0xff, 0xfe, b'a', 0x80, 0xc2];
        let decoded = decode_console_bytes(&bytes);
        assert!(decoded.contains('a'));
    }

    #[test]
    fn decodes_gbk_log_lines_when_utf8_fails() {
        // GBK bytes for "[11:31:38 WARN]: 服务器已启动"
        let mut bytes = b"[11:31:38 WARN]: ".to_vec();
        bytes.extend([
            0xb7, 0xfe, 0xce, 0xf1, 0xc6, 0xf7, 0xd2, 0xd1, 0xc6, 0xf4, 0xb6, 0xaf,
        ]);
        assert_eq!(
            decode_console_bytes(&bytes),
            "[11:31:38 WARN]: 服务器已启动"
        );
    }

    #[test]
    fn decodes_shift_jis_log_lines_when_utf8_fails() {
        // Shift_JIS bytes for "サーバー起動" — detection must not be
        // hard-wired to any single locale's code page.
        let mut bytes = b"[INFO]: ".to_vec();
        bytes.extend([
            0x83, 0x54, 0x81, 0x5b, 0x83, 0x6f, 0x81, 0x5b, 0x8b, 0x4e, 0x93, 0xae,
        ]);
        assert_eq!(decode_console_bytes(&bytes), "[INFO]: サーバー起動");
    }

    #[test]
    fn stream_reassembles_utf8_split_across_chunks() {
        let mut decoder = StreamDecoder::new();
        let bytes = "服务器".as_bytes();
        let mut output = String::new();
        output.push_str(&decoder.push(&bytes[..4]));
        output.push_str(&decoder.push(&bytes[4..]));
        output.push_str(&decoder.flush());
        assert_eq!(output, "服务器");
    }

    #[test]
    fn stream_decodes_gbk_line_split_mid_character() {
        let mut decoder = StreamDecoder::new();
        // GBK "服务器" + newline, split inside the second character.
        let bytes = [0xb7, 0xfe, 0xce, 0xf1, 0xc6, 0xf7, b'\n'];
        let mut output = String::new();
        output.push_str(&decoder.push(&bytes[..3]));
        output.push_str(&decoder.push(&bytes[3..]));
        assert_eq!(output, "服务器\n");
    }

    #[test]
    fn stream_passes_ascii_through_immediately() {
        let mut decoder = StreamDecoder::new();
        assert_eq!(decoder.push(b"> "), "> ");
        assert!(decoder.flush().is_empty());
    }
}
