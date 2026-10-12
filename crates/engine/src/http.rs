//! HTTP plumbing: request construction, response parsing, and the error
//! mapping that turns transport problems into [`Failure`] classes.
//!
//! Nothing in here is user-facing text. See `spec/engine-v1.md` §4 and §8.

use std::time::Duration;

use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderMap, HeaderValue,
    IF_RANGE, LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::{Client, Response, StatusCode};

use crate::failure::{Failure, FailureKind, FailureScope};

/// A byte interval `[start, end]`, both inclusive, together with the complete
/// length the server reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    /// First byte of the interval.
    pub start: u64,
    /// Last byte of the interval, inclusive.
    pub end: u64,
    /// Total length of the representation, when the server stated one.
    pub complete_len: Option<u64>,
}

impl ContentRange {
    /// Number of bytes covered by the interval.
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    /// Whether this interval covers nothing (defensive; `end >= start` always).
    pub const fn is_empty(&self) -> bool {
        self.end < self.start
    }
}

/// Parse a `Content-Range` header in the `bytes start-end/complete` form.
///
/// Only the `bytes` unit is accepted; any other unit, a malformed range, or a
/// missing slash yields `None`, which callers treat as "this response has no
/// usable range information".
pub fn parse_content_range(raw: &str) -> Option<ContentRange> {
    let (unit, rest) = raw.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, complete) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let complete_len = if complete.trim() == "*" {
        None
    } else {
        Some(complete.trim().parse().ok()?)
    };
    Some(ContentRange {
        start,
        end,
        complete_len,
    })
}

/// The value of a `Range: bytes=start-end` request header.
pub fn range_header_value(start: u64, end: u64) -> HeaderValue {
    match HeaderValue::from_str(&format!("bytes={start}-{end}")) {
        Ok(value) => value,
        // Unreachable for digits and a hyphen, but HeaderValue construction
        // must not be able to panic the engine.
        Err(_) => HeaderValue::from_static("bytes=0-0"),
    }
}

/// `Retry-After` in either of its two forms: delta-seconds, or an HTTP date.
///
/// The date form is converted to a delay relative to `now_unix`; a date in the
/// past yields a one-second floor rather than a negative delay.
pub fn parse_retry_after(raw: &str, now_unix: u64) -> Option<Duration> {
    let trimmed = raw.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate_to_unix(trimmed)?;
    Some(Duration::from_secs(when.saturating_sub(now_unix).max(1)))
}

/// Minimal IMF-fixdate parser: `Sun, 06 Nov 1994 08:49:37 GMT`.
fn httpdate_to_unix(raw: &str) -> Option<u64> {
    let rest = raw.split_once(", ").map(|(_, rest)| rest).unwrap_or(raw);
    let mut parts = rest.split_whitespace();
    let day: u64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Validators reported by a source, used for change detection and for the M3
/// consistency judgement. Present here so the field shape does not change later.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Validators {
    /// Strong validator (`ETag`, unquoted), when the server gave a non-weak one.
    pub strong: Option<String>,
    /// Weak validator: a weak `ETag`, or `Last-Modified`.
    pub weak: Option<String>,
}

impl Validators {
    /// Read whatever validators the headers carry.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut strong = None;
        let mut weak = None;
        if let Some(etag) = headers.get(ETAG).and_then(|v| v.to_str().ok()) {
            let trimmed = etag.trim();
            if let Some(stripped) = trimmed.strip_prefix("W/") {
                weak = Some(stripped.trim_matches('"').to_string());
            } else {
                strong = Some(trimmed.trim_matches('"').to_string());
            }
        }
        if weak.is_none()
            && let Some(modified) = headers.get(LAST_MODIFIED).and_then(|v| v.to_str().ok())
        {
            weak = Some(modified.trim().to_string());
        }
        Self { strong, weak }
    }
}

/// What a probe learned. All values are optional because a source is allowed to
/// tell us very little; the caller decides what to do about that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// Total length of the representation, when the response revealed one.
    pub total_len: Option<u64>,
    /// Whether the response actually honoured a range request.
    pub range_supported: bool,
    /// Final URL after redirects. Subsequent requests are issued against this.
    pub final_url: reqwest::Url,
    /// `Content-Disposition`, raw.
    pub content_disposition: Option<String>,
    /// Validators from the response headers.
    pub validators: Validators,
    /// `Accept-Ranges` as declared. Recorded for diagnostics only — the
    /// decision to segment is made from `range_supported`, never from this.
    pub declared_accept_ranges: Option<String>,
}

/// Build the shared client.
///
/// No whole-request timeout: a large transfer legitimately runs for minutes.
/// Instead a connect timeout and an inactivity read timeout bound the damage
/// from a dead peer. Automatic decompression is off (the relevant features are
/// not enabled), so `Content-Length` and `Content-Range` always describe the
/// bytes on the wire and byte offsets stay meaningful.
pub fn build_client() -> Result<Client, Failure> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|error| classify(&error, FailureScope::Probe, "client_build"))
}

/// Issue the metadata probe: a `GET` whose `Range` asks for a single byte.
///
/// A GET rather than a HEAD because the spec requires that range support be
/// established from the actual response, and a HEAD response never carries a
/// `Content-Range`. The body of the one-byte reply is dropped.
pub async fn probe(client: &Client, url: &reqwest::Url, total_timeout: Duration) -> Result<ProbeOutcome, Failure> {
    let request = client
        .get(url.clone())
        .header(RANGE, range_header_value(0, 0));
    let response = match tokio::time::timeout(total_timeout, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(classify(&error, FailureScope::Probe, "probe_send")),
        Err(_) => {
            return Err(Failure::new(
                FailureKind::TransportReset,
                FailureScope::Probe,
                "probe_timeout",
            ));
        }
    };
    interpret_probe(response)
}

/// Turn a probe response into a [`ProbeOutcome`] or a classified failure.
pub fn interpret_probe(response: Response) -> Result<ProbeOutcome, Failure> {
    let status = response.status();
    let headers = response.headers().clone();
    let final_url = response.url().clone();

    let declared_accept_ranges = headers
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string());
    let content_disposition = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string());
    let validators = Validators::from_headers(&headers);
    let content_range = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range);
    let content_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());

    if status.is_redirection() {
        // The redirect policy should have followed it; a redirect that survives
        // means the chain was longer than the policy allows.
        return Err(Failure::new(
            FailureKind::DataMismatch,
            FailureScope::Probe,
            "redirect_unresolved",
        )
        .with_status(status.as_u16()));
    }

    if let Some(failure) = status_failure(status, &headers, FailureScope::Probe) {
        return Err(failure);
    }

    // A 206 whose interval and total are both as requested is the only thing
    // that counts as proof of range support.
    let range_supported = status == StatusCode::PARTIAL_CONTENT
        && content_range
            .map(|range| range.start == 0 && range.complete_len.is_some())
            .unwrap_or(false);

    let total_len = if range_supported {
        content_range.and_then(|range| range.complete_len)
    } else if status == StatusCode::OK {
        // The server ignored the range request and is sending the whole thing.
        content_length
    } else {
        content_length
    };

    Ok(ProbeOutcome {
        total_len,
        range_supported,
        final_url,
        content_disposition,
        validators,
        declared_accept_ranges,
    })
}

/// Map a non-success status onto a failure class. Returns `None` for 2xx.
pub fn status_failure(status: StatusCode, headers: &HeaderMap, scope: FailureScope) -> Option<Failure> {
    if status.is_success() {
        return None;
    }
    let code = status.as_u16();
    let kind = match code {
        401 | 403 | 407 => FailureKind::AccessDenied,
        404 | 410 | 451 => FailureKind::SourceMissing,
        408 | 425 => FailureKind::TransportReset,
        429 => FailureKind::RateLimited,
        416 => FailureKind::DataMismatch,
        500..=599 => FailureKind::ServerError,
        400..=499 => FailureKind::Unknown,
        _ => FailureKind::Unknown,
    };
    let retry_after = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now_unix()));
    let mut failure = Failure::new(kind, scope, format!("http_{code}"))
        .with_status(code)
        .with_retry_after(retry_after);
    // A 429 without a stated wait is still retryable; a 403 never is.
    if code == 408 || code == 425 {
        failure = failure.with_retry_after(None);
    }
    Some(failure)
}

/// Current Unix time in seconds.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs())
        .unwrap_or_default()
}

/// Map a transport error onto a failure class.
///
/// Order matters: the most specific signals are tested first, because a
/// request can be both "a connect error" and "a timeout".
pub fn classify(error: &reqwest::Error, scope: FailureScope, detail: &str) -> Failure {
    let kind = if error.is_timeout() {
        FailureKind::TransportReset
    } else if error.is_connect() {
        FailureKind::TransportReset
    } else if error.is_body() || error.is_decode() {
        FailureKind::DataMismatch
    } else if error.is_redirect() {
        FailureKind::DataMismatch
    } else if error.is_builder() {
        FailureKind::Unknown
    } else {
        FailureKind::Unknown
    };
    let mut failure = Failure::new(kind, scope, detail);
    if let Some(status) = error.status() {
        failure = failure.with_status(status.as_u16());
    }
    failure
}

/// Classify a local I/O failure. Storage is split into "out of space" and
/// "everything else" because only the first is worth a distinct host message.
pub fn classify_io(error: &std::io::Error, detail: &str) -> Failure {
    let kind = match error.kind() {
        std::io::ErrorKind::StorageFull => FailureKind::DiskFull,
        std::io::ErrorKind::PermissionDenied => FailureKind::DiskOther,
        std::io::ErrorKind::InvalidFilename | std::io::ErrorKind::NotFound => FailureKind::DiskOther,
        _ => {
            // Windows reports the out-of-space condition as ERROR_DISK_FULL
            // (112) or ERROR_HANDLE_DISK_FULL (39) on some paths.
            #[cfg(windows)]
            {
                match error.raw_os_error() {
                    Some(39) | Some(112) => FailureKind::DiskFull,
                    _ => FailureKind::DiskOther,
                }
            }
            #[cfg(not(windows))]
            {
                match error.raw_os_error() {
                    Some(28) => FailureKind::DiskFull,
                    _ => FailureKind::DiskOther,
                }
            }
        }
    };
    let mut failure = Failure::new(kind, FailureScope::Storage, detail);
    if let Some(code) = error.raw_os_error() {
        failure = failure.with_status(0).with_status(code as u16);
    }
    failure
}

/// Add a conditional header so that a changed resource is detectable.
pub fn conditional_headers(validators: &Validators) -> Option<HeaderValue> {
    validators
        .strong
        .as_ref()
        .and_then(|tag| HeaderValue::from_str(&format!("\"{tag}\"")).ok())
}

/// Header name used for the conditional request. Separate so tests can assert
/// on it without reaching into reqwest.
pub const CONDITIONAL_HEADER: reqwest::header::HeaderName = IF_RANGE;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use reqwest::header::HeaderMap;

    #[test]
    fn parses_well_formed_content_range() {
        let range = parse_content_range("bytes 0-1023/2048").unwrap();
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 1023);
        assert_eq!(range.complete_len, Some(2048));
        assert_eq!(range.len(), 1024);

        let range = parse_content_range("bytes 100-199/*").unwrap();
        assert_eq!((range.start, range.end, range.complete_len), (100, 199, None));

        let range = parse_content_range("BYTES 5-5/10").unwrap();
        assert_eq!(range.len(), 1);
    }

    #[test]
    fn rejects_malformed_content_range() {
        for raw in [
            "",
            "bytes",
            "bytes 0-1023",
            "items 0-1023/2048",
            "bytes 10-5/2048",
            "bytes a-b/2048",
            "bytes 0-1023/abc",
        ] {
            assert!(parse_content_range(raw).is_none(), "{raw} should not parse");
        }
    }

    #[test]
    fn retry_after_delta_seconds() {
        assert_eq!(
            parse_retry_after("120", 1_000),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 7 ", 1_000), Some(Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_http_date() {
        // 1994-11-06 08:49:37 UTC == 784111777
        let when = httpdate_to_unix("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(when, 784_111_777);
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:47 GMT", 784_111_777),
            Some(Duration::from_secs(10))
        );
        assert_eq!(parse_retry_after("Sun, 06 Foo 1994 08:49:37 GMT", 0), None);
    }

    #[test]
    fn status_classes_map_to_the_right_failure() {
        let empty = HeaderMap::new();
        let cases = [
            (404, FailureKind::SourceMissing, false),
            (410, FailureKind::SourceMissing, false),
            (403, FailureKind::AccessDenied, false),
            (401, FailureKind::AccessDenied, false),
            (429, FailureKind::RateLimited, true),
            (500, FailureKind::ServerError, true),
            (503, FailureKind::ServerError, true),
            (408, FailureKind::TransportReset, true),
            (416, FailureKind::DataMismatch, true),
        ];
        for (code, kind, retryable) in cases {
            let status = StatusCode::from_u16(code).unwrap();
            let failure = status_failure(status, &empty, FailureScope::Transfer).unwrap();
            assert_eq!(failure.kind, kind, "status {code}");
            assert_eq!(failure.retryable, retryable, "status {code}");
            assert_eq!(failure.status, Some(code));
        }
        assert!(status_failure(StatusCode::OK, &empty, FailureScope::Probe).is_none());
        assert!(status_failure(StatusCode::PARTIAL_CONTENT, &empty, FailureScope::Probe).is_none());
    }

    #[test]
    fn throttling_response_keeps_the_server_wait() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("42"));
        let failure =
            status_failure(StatusCode::TOO_MANY_REQUESTS, &headers, FailureScope::Probe).unwrap();
        assert_eq!(failure.retry_after, Some(Duration::from_secs(42)));
    }

    #[test]
    fn validators_split_strong_from_weak() {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("\"abc123\""));
        let validators = Validators::from_headers(&headers);
        assert_eq!(validators.strong.as_deref(), Some("abc123"));
        assert_eq!(validators.weak, None);

        let mut headers = HeaderMap::new();
        headers.insert(ETAG, HeaderValue::from_static("W/\"weak1\""));
        headers.insert(
            LAST_MODIFIED,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        let validators = Validators::from_headers(&headers);
        assert_eq!(validators.strong, None);
        assert_eq!(validators.weak.as_deref(), Some("weak1"));

        let mut headers = HeaderMap::new();
        headers.insert(
            LAST_MODIFIED,
            HeaderValue::from_static("Sun, 06 Nov 1994 08:49:37 GMT"),
        );
        let validators = Validators::from_headers(&headers);
        assert_eq!(
            validators.weak.as_deref(),
            Some("Sun, 06 Nov 1994 08:49:37 GMT")
        );
    }

    #[test]
    fn range_header_value_is_well_formed() {
        assert_eq!(range_header_value(0, 0).to_str().unwrap(), "bytes=0-0");
        assert_eq!(
            range_header_value(1024, 2047).to_str().unwrap(),
            "bytes=1024-2047"
        );
    }

    #[test]
    fn io_errors_split_space_from_the_rest() {
        let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert_eq!(classify_io(&full, "write").kind, FailureKind::DiskFull);
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let failure = classify_io(&denied, "write");
        assert_eq!(failure.kind, FailureKind::DiskOther);
        assert!(!failure.retryable);
    }
}
