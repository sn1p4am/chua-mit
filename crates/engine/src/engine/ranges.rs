//! The `spec/engine-v1.md` §6.3 guard: does this response actually carry the
//! byte interval we asked for?
//!
//! This module exists because of the worst failure mode in the engine. If a
//! server answers a `Range: bytes=A-B` request with `200 OK` and the whole file,
//! and the reader writes that body at offset `A`, the resulting file is silently
//! corrupt — every later byte is wrong, every checksum disagrees, and nothing
//! in the logs says why. So the decision is a separate, total function with its
//! own tests, and the transfer loop has no path that bypasses it.
//!
//! This function never writes anything. Its only job is to say yes or no.

use crate::failure::{Failure, FailureKind, FailureScope};
use crate::http::ContentRange;

/// What the caller asked for, and the shape of file it believes it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeRequest {
    /// First byte requested, inclusive.
    pub start: u64,
    /// Last byte requested, inclusive.
    pub end: u64,
    /// The length the engine believes the whole file has. `None` is the
    /// unknown-length mode of §4.3, which necessarily uses a whole-file
    /// request.
    pub expected_total: Option<u64>,
}

impl RangeRequest {
    /// A request for exactly `[start, end]`.
    pub const fn new(start: u64, end: u64, expected_total: Option<u64>) -> Self {
        Self {
            start,
            end,
            expected_total,
        }
    }

    /// A request for the entire file. This is the only request shape for which
    /// a `200` answer is legitimate: the range *is* the whole file.
    pub const fn whole_file(total_len: Option<u64>) -> Self {
        Self {
            start: 0,
            end: u64::MAX,
            expected_total: total_len,
        }
    }

    /// A request for one interval of a segmented transfer.
    pub const fn segment(start: u64, end: u64, total_len: u64) -> Self {
        Self {
            start,
            end,
            expected_total: Some(total_len),
        }
    }

    /// Bytes requested.
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    /// Whether this request covers `[0, total)` and therefore may legitimately
    /// be answered with the whole representation.
    pub fn covers_whole_file(&self) -> bool {
        if self.start != 0 {
            return false;
        }
        match self.expected_total {
            Some(total) => self.end.saturating_add(1) >= total,
            // Unknown length: the reader takes whatever arrives, so a `200` is
            // exactly the expected answer.
            None => self.end == u64::MAX,
        }
    }
}

/// The verdict on a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeVerdict {
    /// `206` whose interval and total match the request. Safe to write at
    /// `request.start`.
    Accept,
    /// `200 OK` for a request that asked for the whole file. Safe to write from
    /// offset 0, and the full content length was recorded.
    AcceptWholeFile {
        /// Length the response claims, when it stated one.
        content_len: Option<u64>,
    },
    /// `200 OK` for a request that asked for part of the file. The server
    /// ignored `Range`. Under no circumstances may this body be written at
    /// `request.start`.
    ServerIgnoredRange {
        /// Length the response claims, when it stated one.
        content_len: Option<u64>,
    },
    /// `206` whose interval is not the one we asked for, or whose total
    /// disagrees with what we already believe.
    RangeMismatch {
        /// Short machine-readable token, e.g. `partial_start_100_expected_0`.
        detail: String,
    },
    /// Any other status. Classified by the caller from the status alone.
    StatusRejected {
        /// The status code.
        status: u16,
    },
}

impl RangeVerdict {
    /// Whether the body may be written.
    pub const fn is_writable(&self) -> bool {
        matches!(self, Self::Accept | Self::AcceptWholeFile { .. })
    }

    /// The offset the body starts at, when it may be written.
    ///
    /// A whole-file `200` always starts at zero; a `206` starts where it says.
    pub const fn write_offset(&self, request: &RangeRequest) -> Option<u64> {
        match self {
            Self::Accept => Some(request.start),
            Self::AcceptWholeFile { .. } => Some(0),
            _ => None,
        }
    }
}

/// Decide whether a segment response may be written.
///
/// `status` is the raw status code; `content_range` is the parsed
/// `Content-Range` header (already percent-of-nothing: `None` when absent or
/// malformed); `content_len` is the `Content-Length` header, which for a `206`
/// is the length of the *part*, not of the whole file.
///
/// A `200` is never accepted for a partial request, and a `206` is never
/// accepted unless its interval is exactly the requested one and its total
/// agrees with the engine's own idea of the file size.
pub fn validate_range_response(
    request: &RangeRequest,
    status: u16,
    content_range: Option<ContentRange>,
    content_len: Option<u64>,
) -> RangeVerdict {
    match status {
        206 => match content_range {
            None => RangeVerdict::RangeMismatch {
                detail: "partial_without_content_range".to_string(),
            },
            Some(range) => {
                if range.start != request.start {
                    return RangeVerdict::RangeMismatch {
                        detail: format!("partial_start_{}_expected_{}", range.start, request.start),
                    };
                }
                if request.end != u64::MAX && range.end != request.end {
                    return RangeVerdict::RangeMismatch {
                        detail: format!("partial_end_{}_expected_{}", range.end, request.end),
                    };
                }
                if let (Some(reported), Some(expected)) = (range.complete_len, request.expected_total)
                    && reported != expected
                {
                    return RangeVerdict::RangeMismatch {
                        detail: format!("total_changed_{reported}_was_{expected}"),
                    };
                }
                // A short part is what a range clamp looks like; the caller
                // notices the early end of body and re-plans.
                RangeVerdict::Accept
            }
        },
        200 => {
            if request.covers_whole_file() {
                RangeVerdict::AcceptWholeFile { content_len }
            } else {
                RangeVerdict::ServerIgnoredRange { content_len }
            }
        }
        other => RangeVerdict::StatusRejected { status: other },
    }
}

/// Turn a non-writable verdict into the structured failure the host sees.
///
/// Both `ServerIgnoredRange` and `RangeMismatch` are classed `DataMismatch`,
/// which §8.2 marks retryable *against a different source or after a re-probe*.
/// The failure is therefore returned with `retryable = false` for the
/// "repeat the identical request" reading, which is the one the transfer loop
/// must not take: the caller's remedy is a re-probe or a source change, not
/// another request for the same bytes to the same endpoint.
pub fn verdict_failure(verdict: &RangeVerdict, scope: FailureScope) -> Option<Failure> {
    match verdict {
        RangeVerdict::Accept | RangeVerdict::AcceptWholeFile { .. } => None,
        RangeVerdict::ServerIgnoredRange { content_len } => Some(
            Failure::new(
                FailureKind::DataMismatch,
                scope,
                match content_len {
                    Some(len) => format!("server_ignored_range_full_body_{len}"),
                    None => "server_ignored_range".to_string(),
                },
            )
            .not_retryable(),
        ),
        RangeVerdict::RangeMismatch { detail } => Some(
            Failure::new(FailureKind::DataMismatch, scope, detail.clone()).not_retryable(),
        ),
        RangeVerdict::StatusRejected { status } => {
            let code = u16::try_from(*status).unwrap_or(u16::MAX);
            reqwest::StatusCode::from_u16(code)
                .ok()
                .and_then(|status| crate::http::status_failure(status, &Default::default(), scope))
                .or_else(|| {
                    Some(Failure::new(FailureKind::Unknown, scope, format!("http_{code}")))
                })
        }
    }
}

/// How many leading bytes two responses share. Used by the transfer loop to
/// salvage an already-open body after a re-plan, and by the re-plan path to
/// prove that no byte was written out of place.
///
/// Kept here, next to the guard, because both callers must agree on what
/// "identical prefix" means.
pub fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn cr(start: u64, end: u64, complete: Option<u64>) -> ContentRange {
        ContentRange {
            start,
            end,
            complete_len: complete,
        }
    }

    #[test]
    fn matching_partial_response_is_accepted() {
        let request = RangeRequest::segment(1024, 2047, 8192);
        let verdict = validate_range_response(&request, 206, Some(cr(1024, 2047, Some(8192))), Some(1024));
        assert_eq!(verdict, RangeVerdict::Accept);
        assert!(verdict.is_writable());
        assert_eq!(verdict.write_offset(&request), Some(1024));
        assert!(verdict_failure(&verdict, FailureScope::Transfer).is_none());
    }

    #[test]
    fn the_whole_point_a_200_for_a_partial_request_is_never_writable() {
        // The dangerous case, stated as plainly as possible: we asked for
        // bytes 1024..2047, the server sent the entire file with 200.
        let request = RangeRequest::segment(1024, 2047, 8192);
        let verdict = validate_range_response(&request, 200, None, Some(8192));
        assert_eq!(
            verdict,
            RangeVerdict::ServerIgnoredRange {
                content_len: Some(8192)
            }
        );
        assert!(!verdict.is_writable());
        assert_eq!(verdict.write_offset(&request), None);

        let failure = verdict_failure(&verdict, FailureScope::Transfer).unwrap();
        assert_eq!(failure.kind, FailureKind::DataMismatch);
        assert_eq!(failure.detail, "server_ignored_range_full_body_8192");
    }

    #[test]
    fn a_200_is_accepted_only_for_a_whole_file_request() {
        // Unknown-length mode: the reader asked for everything it could.
        let request = RangeRequest::whole_file(None);
        assert!(request.covers_whole_file());
        let verdict = validate_range_response(&request, 200, None, Some(4096));
        assert_eq!(
            verdict,
            RangeVerdict::AcceptWholeFile {
                content_len: Some(4096)
            }
        );
        assert!(verdict.is_writable());
        assert_eq!(verdict.write_offset(&request), Some(0));

        // Known length, single segment covering all of it.
        let request = RangeRequest::segment(0, 4095, 4096);
        assert!(request.covers_whole_file());
        assert!(validate_range_response(&request, 200, None, Some(4096)).is_writable());

        // Known length, first segment of many: `bytes=0-1023` does not cover the
        // file, so a 200 is still the ignored-range case even though the offset
        // happens to be zero.
        let request = RangeRequest::segment(0, 1023, 4096);
        assert!(!request.covers_whole_file());
        assert!(!validate_range_response(&request, 200, None, Some(4096)).is_writable());
    }

    #[test]
    fn a_200_without_a_length_is_still_not_writable_for_a_partial_request() {
        let request = RangeRequest::segment(500, 999, 4096);
        let verdict = validate_range_response(&request, 200, None, None);
        assert_eq!(
            verdict,
            RangeVerdict::ServerIgnoredRange { content_len: None }
        );
        assert!(!verdict.is_writable());
    }

    #[test]
    fn partial_responses_with_the_wrong_interval_are_rejected() {
        let request = RangeRequest::segment(0, 1023, 4096);
        // The server sent the whole file as a 206 (some do).
        let verdict = validate_range_response(&request, 206, Some(cr(0, 4095, Some(4096))), Some(4096));
        assert_eq!(
            verdict,
            RangeVerdict::RangeMismatch {
                detail: "partial_end_4095_expected_1023".to_string()
            }
        );
        assert!(!verdict.is_writable());

        // Off-by-one start.
        let request = RangeRequest::segment(1024, 2047, 4096);
        let verdict = validate_range_response(&request, 206, Some(cr(1023, 2047, Some(4096))), Some(1025));
        assert_eq!(
            verdict,
            RangeVerdict::RangeMismatch {
                detail: "partial_start_1023_expected_1024".to_string()
            }
        );
    }

    #[test]
    fn a_206_without_content_range_is_rejected() {
        let request = RangeRequest::segment(0, 99, 1000);
        let verdict = validate_range_response(&request, 206, None, Some(100));
        assert_eq!(
            verdict,
            RangeVerdict::RangeMismatch {
                detail: "partial_without_content_range".to_string()
            }
        );
        assert!(!verdict.is_writable());
    }

    #[test]
    fn a_changed_total_is_rejected() {
        let request = RangeRequest::segment(0, 99, 1000);
        let verdict = validate_range_response(&request, 206, Some(cr(0, 99, Some(2000))), Some(100));
        assert_eq!(
            verdict,
            RangeVerdict::RangeMismatch {
                detail: "total_changed_2000_was_1000".to_string()
            }
        );
        assert!(!verdict.is_writable());
        let failure = verdict_failure(&verdict, FailureScope::Transfer).unwrap();
        assert_eq!(failure.kind, FailureKind::DataMismatch);
    }

    #[test]
    fn a_partial_response_may_report_an_unknown_total() {
        let request = RangeRequest::segment(0, 99, 1000);
        let verdict = validate_range_response(&request, 206, Some(cr(0, 99, None)), Some(100));
        assert_eq!(verdict, RangeVerdict::Accept);
    }

    #[test]
    fn error_statuses_are_reported_not_written() {
        let request = RangeRequest::segment(0, 99, 1000);
        for (status, kind) in [
            (404, FailureKind::SourceMissing),
            (403, FailureKind::AccessDenied),
            (429, FailureKind::RateLimited),
            (500, FailureKind::ServerError),
            (416, FailureKind::DataMismatch),
        ] {
            let verdict = validate_range_response(&request, status, None, None);
            assert_eq!(verdict, RangeVerdict::StatusRejected { status });
            assert!(!verdict.is_writable());
            let failure = verdict_failure(&verdict, FailureScope::Transfer).unwrap();
            assert_eq!(failure.kind, kind, "status {status}");
            assert_eq!(failure.status, Some(status));
        }
    }

    #[test]
    fn no_writable_verdict_ever_points_at_a_nonzero_offset_for_a_degraded_response() {
        // Exhaustive over the interesting status shapes: the only writable
        // outcomes are a matching 206 and a whole-file 200 at offset 0.
        let requests = [
            RangeRequest::segment(0, 1023, 4096),
            RangeRequest::segment(1024, 2047, 4096),
            RangeRequest::segment(0, 4095, 4096),
            RangeRequest::whole_file(None),
            RangeRequest::whole_file(Some(4096)),
        ];
        let ranges = [
            None,
            Some(cr(0, 1023, Some(4096))),
            Some(cr(1024, 2047, Some(4096))),
            Some(cr(0, 4095, Some(4096))),
        ];
        for request in requests {
            for status in [200u16, 206, 404, 416, 500] {
                for range in ranges {
                    let verdict = validate_range_response(&request, status, range, Some(1024));
                    if let Some(offset) = verdict.write_offset(&request) {
                        assert!(verdict.is_writable());
                        match verdict {
                            RangeVerdict::Accept => {
                                assert_eq!(status, 206);
                                assert_eq!(offset, request.start);
                                assert_eq!(range.unwrap().start, request.start);
                            }
                            RangeVerdict::AcceptWholeFile { .. } => {
                                assert_eq!(status, 200);
                                assert_eq!(offset, 0);
                                assert!(request.covers_whole_file());
                            }
                            other => panic!("writable verdict {other:?}"),
                        }
                    } else {
                        assert!(!verdict.is_writable());
                    }
                }
            }
        }
    }

    #[test]
    fn request_shapes_report_coverage_correctly() {
        assert!(RangeRequest::whole_file(None).covers_whole_file());
        assert!(RangeRequest::whole_file(Some(10)).covers_whole_file());
        assert!(RangeRequest::segment(0, 4095, 4096).covers_whole_file());
        assert!(!RangeRequest::segment(1, 4095, 4096).covers_whole_file());
        assert!(!RangeRequest::segment(0, 4094, 4096).covers_whole_file());
        assert_eq!(RangeRequest::segment(10, 19, 100).len(), 10);
        assert_eq!(RangeRequest::whole_file(Some(4096)).len(), u64::MAX);
    }

    #[test]
    fn common_prefix_len_is_exact() {
        assert_eq!(common_prefix_len(b"abcdef", b"abcxyz"), 3);
        assert_eq!(common_prefix_len(b"abc", b"abc"), 3);
        assert_eq!(common_prefix_len(b"", b"abc"), 0);
        assert_eq!(common_prefix_len(b"x", b"y"), 0);
    }
}
