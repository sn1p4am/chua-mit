//! Percent decoding. Small and byte-oriented, so that malformed input is
//! preserved rather than dropped.
//!
//! Kept in-tree on purpose: the sanitisation path is security relevant and the
//! policy for malformed sequences has to be explicit and testable.

/// Decode `%XX` escapes as UTF-8 bytes.
///
/// * A valid escape is decoded to its byte.
/// * An invalid or truncated escape (`%`, `%z1`, `%4`) is copied through
///   verbatim — refusing to guess keeps the result predictable.
/// * Decoded bytes are re-interpreted as UTF-8. If the byte sequence is not
///   valid UTF-8 the original (still encoded) input is returned, so a hostile
///   name cannot smuggle in a replacement character that changes its meaning.
pub fn decode_percent(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                (Some(high), Some(low)) => {
                    out.push((high << 4) | low);
                    index += 3;
                    continue;
                }
                _ => {
                    out.push(bytes[index]);
                    index += 1;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// Decode one percent-escape sequence that is known to be well formed.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn decodes_ascii_escapes() {
        assert_eq!(decode_percent("a%20b"), "a b");
        assert_eq!(decode_percent("%2e%2e"), "..");
        assert_eq!(decode_percent("%2Fetc%2Fpasswd"), "/etc/passwd");
    }

    #[test]
    fn decodes_multi_byte_utf8() {
        assert_eq!(decode_percent("%E6%96%87%E4%BB%B6.zip"), "文件.zip");
    }

    #[test]
    fn leaves_malformed_escapes_alone() {
        assert_eq!(decode_percent("100%"), "100%");
        assert_eq!(decode_percent("%zz"), "%zz");
        assert_eq!(decode_percent("%4"), "%4");
        assert_eq!(decode_percent("a%2"), "a%2");
    }

    #[test]
    fn leaves_invalid_utf8_alone_rather_than_mangling_it() {
        // 0xFF is not valid UTF-8 on its own.
        assert_eq!(decode_percent("%FF%FE"), "%FF%FE");
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(decode_percent("%2f"), decode_percent("%2F"));
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(decode_percent("plain-name.tar.gz"), "plain-name.tar.gz");
        assert_eq!(decode_percent(""), "");
    }
}
