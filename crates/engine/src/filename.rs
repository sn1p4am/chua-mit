//! Final-filename derivation and sanitisation — `spec/engine-v1.md` §4.2.
//!
//! The order is fixed by the spec: content-disposition header, then the last
//! segment of the final URL path, then the name the host supplied. The winner
//! is *always* run through [`sanitize`]; nothing reaches the filesystem raw.

use url::Url;

use crate::util::percent::decode_percent;

/// Cap on a single path component, in bytes. Windows' own limit is 255
/// UTF-16 code units; 200 bytes leaves room for a disambiguating suffix.
const MAX_COMPONENT_BYTES: usize = 200;

/// Name used when nothing usable survives sanitisation.
const FALLBACK_STEM: &str = "download";

/// Windows device names. The check is on the stem before the first dot, and is
/// case-insensitive.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Why a raw name was rewritten. Machine-readable only; the host owns wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameRejection {
    /// Control characters or characters illegal in a Windows path component.
    IllegalCharacters,
    /// A path separator was present, so the candidate tried to name a path.
    SeparatorsRemoved,
    /// Trailing dots or spaces had to be trimmed.
    TrailingTrimmed,
    /// A Windows device name (`CON`, `LPT1`, ...).
    Reserved,
}

/// A filename that has been validated against the filesystem's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeName(String);

impl SafeName {
    /// The sanitised name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume and return the name.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for SafeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Outcome of sanitising a candidate name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sanitized {
    /// The candidate was usable as-is.
    Acceptable(SafeName),
    /// The candidate had to be rewritten.
    Rewritten {
        /// The rewritten name.
        name: SafeName,
        /// What was wrong with the input.
        reason: NameRejection,
    },
    /// The candidate was unusable and the fallback was substituted.
    Fallback(SafeName),
}

impl Sanitized {
    /// The usable name, whichever branch was taken.
    pub fn name(&self) -> &SafeName {
        match self {
            Self::Acceptable(name) | Self::Rewritten { name, .. } | Self::Fallback(name) => name,
        }
    }

    /// Consume and return the usable name.
    pub fn into_name(self) -> SafeName {
        match self {
            Self::Acceptable(name) | Self::Rewritten { name, .. } | Self::Fallback(name) => name,
        }
    }

    /// Whether the name had to be changed.
    pub fn was_rewritten(&self) -> bool {
        !matches!(self, Self::Acceptable(_))
    }
}

/// Derivation sources, in the spec's order of precedence.
#[derive(Debug, Clone)]
pub struct NameSources<'a> {
    /// Raw `Content-Disposition` header value, if the response had one.
    pub content_disposition: Option<&'a str>,
    /// The URL the response finally came from, after redirects.
    pub final_url: &'a Url,
    /// A name the host supplied (`add <url> <dest>`), lowest precedence.
    pub host_hint: Option<&'a str>,
}

/// Pick the first available source and sanitise it.
///
/// Sources that exist but sanitise to nothing usable do not stop the search:
/// the next source is tried, and the fallback is used only if every source runs
/// out. This keeps a hostile `Content-Disposition` from forcing an empty path.
pub fn derive(sources: &NameSources<'_>) -> Sanitized {
    let mut first: Option<Sanitized> = None;

    if let Some(header) = sources.content_disposition
        && let Some(raw) = parse_content_disposition(header)
    {
        let outcome = sanitize(&raw);
        if !matches!(outcome, Sanitized::Fallback(_)) {
            return outcome;
        }
        first = Some(outcome);
    }

    if let Some(segment) = last_path_segment(sources.final_url) {
        let outcome = sanitize(&segment);
        if !matches!(outcome, Sanitized::Fallback(_)) {
            return outcome;
        }
        first = first.or(Some(outcome));
    }

    if let Some(hint) = sources.host_hint {
        let outcome = sanitize(hint);
        if !matches!(outcome, Sanitized::Fallback(_)) {
            return outcome;
        }
        first = first.or(Some(outcome));
    }

    first.unwrap_or_else(|| Sanitized::Fallback(SafeName(FALLBACK_STEM.to_string())))
}

/// The last path segment of a URL, percent-decoded, with the query string
/// already excluded by the URL parser.
pub fn last_path_segment(url: &Url) -> Option<String> {
    let segments: Vec<&str> = url.path_segments()?.collect();
    let last = segments.iter().rev().find(|segment| !segment.is_empty())?;
    Some(decode_percent(last))
}

/// Extract the filename from a `Content-Disposition` header value.
///
/// Handles both the plain `filename="..."` form and the RFC 5987 extended
/// `filename*=charset'language'value` form. The extended form wins where both
/// are present, because it is the one that can carry a non-ASCII name.
///
/// Returns `None` when the header carries no filename at all.
pub fn parse_content_disposition(header: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for field in split_parameters(header) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.eq_ignore_ascii_case("filename*") {
            if let Some(decoded) = decode_ext_value(value.trim()) {
                extended = Some(decoded);
            }
        } else if key.eq_ignore_ascii_case("filename") {
            plain = Some(unquote(value.trim()).to_string());
        }
    }

    extended.or(plain)
}

/// Split a header value on `;`, ignoring separators inside quoted strings.
fn split_parameters(header: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in header.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => {
                quoted = !quoted;
                current.push(ch);
            }
            ';' if !quoted => {
                fields.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

/// Strip one layer of surrounding double quotes and the backslash escapes that
/// the quoted-string production allows.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let inner = &value[1..value.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for ch in inner.chars() {
            if escaped {
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        value.to_string()
    }
}

/// Decode an RFC 5987 extended value: `charset'language'percent-encoded`.
///
/// Only UTF-8 (and its `utf8` spelling) is honoured; anything else is treated as
/// Latin-1, which is the worst case the RFC allows. A value with no `'`
/// separators is treated as already decoded.
fn decode_ext_value(value: &str) -> Option<String> {
    let value = unquote(value);
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next();
    let payload = match parts.next() {
        Some(payload) => payload,
        None => return Some(value),
    };
    let decoded = decode_percent(payload);
    let lowered = charset.trim().to_ascii_lowercase();
    if lowered.is_empty() || lowered == "utf-8" || lowered == "utf8" {
        return Some(decoded);
    }
    if lowered == "iso-8859-1" || lowered == "latin1" {
        return Some(latin1_to_string(&decoded));
    }
    // Unknown charset: keep the bytes we have rather than dropping the name.
    Some(decoded)
}

fn latin1_to_string(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            let code = ch as u32;
            if code < 0x100 {
                char::from_u32(code).unwrap_or(ch)
            } else {
                ch
            }
        })
        .collect()
}

/// Make an arbitrary candidate safe to use as a single path component.
///
/// Rules, all of them mandatory:
/// * percent-decoding happens first, so `%2e%2e` cannot smuggle `..` past it;
/// * path separators (`/`, `\`) are removed — a candidate can never introduce a
///   directory component;
/// * C0 and C1 control characters and the DEL character are removed;
/// * characters that are illegal in Windows path components are replaced;
/// * trailing dots and spaces are removed;
/// * `.` and `..` are rejected outright;
/// * Windows device names are prefixed so they stop being device names;
/// * the result is truncated on a character boundary.
pub fn sanitize(candidate: &str) -> Sanitized {
    let decoded = decode_percent(candidate);
    let mut cleaned = String::with_capacity(decoded.len());
    let mut reason: Option<NameRejection> = None;

    for ch in decoded.chars() {
        if ch == '/' || ch == '\\' {
            reason = Some(NameRejection::SeparatorsRemoved);
            continue;
        }
        if ch.is_control() {
            reason = Some(NameRejection::IllegalCharacters);
            continue;
        }
        if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            reason = Some(NameRejection::IllegalCharacters);
            cleaned.push('_');
            continue;
        }
        cleaned.push(ch);
    }

    // Trailing dots and spaces are dropped by Windows silently; do it visibly.
    let trimmed_len = cleaned.trim_end_matches([' ', '.']).len();
    if trimmed_len != cleaned.len() {
        reason = Some(NameRejection::TrailingTrimmed);
    }
    cleaned.truncate(trimmed_len);

    // Leading dots would produce a hidden file on POSIX platforms, and a name
    // of nothing but dots is a directory reference whatever the platform.
    if cleaned.starts_with('.') {
        reason = Some(NameRejection::IllegalCharacters);
        cleaned = cleaned.trim_start_matches('.').to_string();
    }

    if cleaned.is_empty() {
        return Sanitized::Fallback(SafeName(FALLBACK_STEM.to_string()));
    }

    let mut out = cleaned;
    if is_reserved(&out) {
        out.insert(0, '_');
        reason = Some(NameRejection::Reserved);
    }
    truncate_component(&mut out);

    let name = SafeName(out);
    match reason {
        Some(reason) => Sanitized::Rewritten { name, reason },
        None => Sanitized::Acceptable(name),
    }
}

/// Windows reserves these stems, with or without an extension.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
}

/// Truncate to [`MAX_COMPONENT_BYTES`] on a character boundary, keeping the
/// extension when there is one.
fn truncate_component(name: &mut String) {
    if name.len() <= MAX_COMPONENT_BYTES {
        return;
    }
    let extension = name
        .rfind('.')
        .filter(|index| *index > 0 && name.len() - index <= 16)
        .map(|index| name[index..].to_string())
        .unwrap_or_default();
    let budget = MAX_COMPONENT_BYTES.saturating_sub(extension.len());
    let mut cut = budget;
    while cut > 0 && !name.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = name[..cut].to_string();
    truncated.push_str(&extension);
    *name = truncated;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn name_of(candidate: &str) -> String {
        sanitize(candidate).into_name().into_string()
    }

    #[test]
    fn plain_name_is_accepted_untouched() {
        let outcome = sanitize("archive-v1.tar.gz");
        assert_eq!(outcome, Sanitized::Acceptable(SafeName("archive-v1.tar.gz".into())));
        assert!(!outcome.was_rewritten());
    }

    #[test]
    fn parent_directory_escape_is_neutralised() {
        // Both the literal and the percent-encoded forms.
        assert_eq!(name_of(".."), "download");
        assert_eq!(name_of("%2e%2e"), "download");
        assert_eq!(name_of("."), "download");
        assert_eq!(name_of("../etc/passwd"), "etc_passwd");
        assert_eq!(name_of("..\\..\\windows\\system32"), "windows_system32");
    }

    #[test]
    fn path_separators_never_survive() {
        for input in ["a/b", "a\\b", "/etc/shadow", "C:\\Windows\\notepad.exe"] {
            let name = name_of(input);
            assert!(!name.contains('/'), "{input} -> {name}");
            assert!(!name.contains('\\'), "{input} -> {name}");
        }
        assert_eq!(name_of("a/b"), "ab");
    }

    #[test]
    fn control_characters_are_stripped() {
        assert_eq!(name_of("bad\u{0}name"), "badname");
        assert_eq!(name_of("a\nb\r\tc"), "abc");
        assert_eq!(name_of("bell\u{7}"), "bell");
        // C1 range.
        assert_eq!(name_of("x\u{85}y"), "xy");
    }

    #[test]
    fn windows_reserved_names_are_prefixed() {
        for reserved in ["CON", "con", "Con", "NUL", "lpt1", "COM9"] {
            let name = name_of(reserved);
            assert!(name.starts_with('_'), "{reserved} -> {name}");
        }
        // With an extension the stem is what matters.
        assert_eq!(name_of("CON.txt"), "_CON.txt");
        assert_eq!(name_of("aux.tar.gz"), "_aux.tar.gz");
        // ... and a name that merely starts with a reserved word is fine.
        assert_eq!(name_of("console.log"), "console.log");
        assert_eq!(name_of("COM10"), "COM10");
    }

    #[test]
    fn trailing_dots_and_spaces_are_removed() {
        assert_eq!(name_of("report.txt."), "report.txt");
        assert_eq!(name_of("report.txt   "), "report.txt");
        assert_eq!(name_of("report.txt. . "), "report.txt");
        assert_eq!(name_of("trailing..."), "trailing");
    }

    #[test]
    fn leading_dots_are_removed() {
        assert_eq!(name_of(".bashrc"), "bashrc");
        assert_eq!(name_of("...hidden"), "hidden");
    }

    #[test]
    fn percent_encoding_is_decoded_before_sanitising() {
        assert_eq!(name_of("my%20file.zip"), "my file.zip");
        assert_eq!(name_of("%E6%96%87%E4%BB%B6.zip"), "文件.zip");
        // Encoded separator: decoded, then removed.
        assert_eq!(name_of("a%2Fb"), "ab");
        assert_eq!(name_of("%2F%2F%2F"), "download");
    }

    #[test]
    fn illegal_windows_characters_are_replaced() {
        assert_eq!(name_of("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
    }

    #[test]
    fn empty_and_separator_only_inputs_fall_back() {
        assert_eq!(name_of(""), "download");
        assert_eq!(name_of("///"), "download");
        assert_eq!(name_of("\u{0}\u{1}"), "download");
        assert_eq!(name_of("   "), "download");
    }

    #[test]
    fn long_names_are_truncated_on_a_boundary() {
        let long = format!("{}.bin", "a".repeat(500));
        let name = name_of(&long);
        assert!(name.len() <= MAX_COMPONENT_BYTES);
        assert!(name.ends_with(".bin"));
        // Multi-byte tail must not be split.
        let cjk = format!("{}.zip", "文".repeat(200));
        let name = name_of(&cjk);
        assert!(name.len() <= MAX_COMPONENT_BYTES);
        assert!(name.ends_with(".zip"));
    }

    #[test]
    fn content_disposition_plain_form() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".to_string())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=report.pdf"),
            Some("report.pdf".to_string())
        );
        assert_eq!(parse_content_disposition("inline"), None);
    }

    #[test]
    fn content_disposition_rfc5987_form() {
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''%E6%96%87%E4%BB%B6.zip"),
            Some("文件.zip".to_string())
        );
        assert_eq!(
            parse_content_disposition("attachment; filename=\"fallback.zip\"; filename*=utf-8''real%20name.zip"),
            Some("real name.zip".to_string())
        );
    }

    #[test]
    fn content_disposition_semicolon_inside_quotes() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"a;b.txt\""),
            Some("a;b.txt".to_string())
        );
    }

    #[test]
    fn content_disposition_hostile_values() {
        // Traversal through the header.
        assert_eq!(
            parse_content_disposition("attachment; filename=\"../../etc/passwd\""),
            Some("../../etc/passwd".to_string())
        );
        let derived = derive(&NameSources {
            content_disposition: Some("attachment; filename=\"../../etc/passwd\""),
            final_url: &url("https://example.test/x"),
            host_hint: None,
        });
        assert_eq!(derived.into_name().into_string(), "etc_passwd");
        // Header that carries no filename falls through to the URL.
        let derived = derive(&NameSources {
            content_disposition: Some("attachment"),
            final_url: &url("https://example.test/dir/file.bin?token=abc"),
            host_hint: None,
        });
        assert_eq!(derived.into_name().into_string(), "file.bin");
    }

    #[test]
    fn mixed_encoding_names() {
        // Encoded CJK plus a literal space plus an encoded separator.
        assert_eq!(name_of("%E6%96%87%E4%BB%B6 %2Fname.zip"), "文件 name.zip");
        // Half-encoded traversal.
        assert_eq!(name_of("..%2F..%2Fboot.ini"), "boot.ini");
        // RFC 5987 payload that is itself double-encoded.
        assert_eq!(
            parse_content_disposition("attachment; filename*=UTF-8''%252Fetc"),
            Some("%2Fetc".to_string())
        );
        assert_eq!(name_of(&parse_content_disposition("attachment; filename*=UTF-8''%252Fetc").unwrap()), "etc");
    }

    #[test]
    fn derivation_precedence() {
        let final_url = url("https://example.test/path/from-url.bin?x=1");
        let from_header = derive(&NameSources {
            content_disposition: Some("attachment; filename=from-header.bin"),
            final_url: &final_url,
            host_hint: Some("from-host.bin"),
        });
        assert_eq!(from_header.into_name().into_string(), "from-header.bin");

        let from_url = derive(&NameSources {
            content_disposition: None,
            final_url: &final_url,
            host_hint: Some("from-host.bin"),
        });
        assert_eq!(from_url.into_name().into_string(), "from-url.bin");

        let from_host = derive(&NameSources {
            content_disposition: None,
            final_url: &url("https://example.test/"),
            host_hint: Some("from-host.bin"),
        });
        assert_eq!(from_host.into_name().into_string(), "from-host.bin");

        let nothing = derive(&NameSources {
            content_disposition: None,
            final_url: &url("https://example.test/"),
            host_hint: None,
        });
        assert_eq!(nothing.into_name().into_string(), "download");
    }

    #[test]
    fn url_path_segment_ignores_query_and_decodes() {
        assert_eq!(
            last_path_segment(&url("https://example.test/a/b/my%20file.zip?sig=1&x=2")).unwrap(),
            "my file.zip"
        );
        assert_eq!(last_path_segment(&url("https://example.test/")), None);
        assert_eq!(last_path_segment(&url("https://example.test")), None);
    }

    #[test]
    fn unusable_header_falls_through_to_the_url() {
        // Every source is unusable: the fallback wins and the search continues.
        let derived = derive(&NameSources {
            content_disposition: Some("attachment; filename=\"..\""),
            final_url: &url("https://example.test/%2e%2e"),
            host_hint: Some("."),
        });
        assert_eq!(derived.into_name().into_string(), "download");
    }
}
