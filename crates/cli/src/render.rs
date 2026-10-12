//! Turning the engine's values into lines a person can read.
//!
//! `spec/engine-v1.md` §8.4 puts wording on the host, and this is where the
//! host does it. The engine's own tokens are kept in the output rather than
//! replaced: they are what correlates a line here with a log line or an event
//! field, and dropping them would make the CLI pleasant at the cost of being
//! diagnosable.
//!
//! Everything here is ASCII on purpose — see the note at the top of `cli.rs`.

use chua_engine::{Failure, FailureKind, TaskView};

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// A byte count with a unit that keeps it short.
pub fn bytes(value: u64) -> String {
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit + 1 < UNITS.len() {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{value} B");
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

/// `done / total (42%)`, or a byte count alone when the length is unknown.
///
/// §4.3 forbids showing a percentage for an unknown-length transfer: there is
/// no denominator, and a number that does not mean anything is worse than no
/// number at all.
pub fn progress(view: &TaskView) -> String {
    match view.total_len {
        Some(total) if total > 0 => {
            let percent = 100.0 * view.received_len as f64 / total as f64;
            format!(
                "{} / {} ({percent:.0}%)",
                bytes(view.received_len),
                bytes(total)
            )
        }
        Some(total) => format!("{} / {}", bytes(view.received_len), bytes(total)),
        None => format!("{} / length unknown", bytes(view.received_len)),
    }
}

/// One task, one line: id, state, progress, destination.
pub fn list_line(view: &TaskView) -> String {
    format!(
        "{:<4} {:<12} {:<30} {}",
        view.id,
        view.state.as_str(),
        progress(view),
        view.target_path
    )
}

/// The detail block for `status`.
pub fn status_block(view: &TaskView) -> String {
    let mut out = String::new();
    out.push_str(&format!("task #{}\n", view.id));
    out.push_str(&format!("  state      {}\n", view.state.as_str()));
    out.push_str(&format!("  source     {}\n", view.source_label));
    out.push_str(&format!("  target     {}\n", view.target_path));
    out.push_str(&format!("  progress   {}\n", progress(view)));
    out.push_str(&format!("  durable    {}\n", bytes(view.done_len)));
    out.push_str(&format!(
        "  rate       {}\n",
        match view.rate_bps {
            Some(rate) if rate > 0 => format!("{}/s", bytes(rate)),
            _ => "unknown".to_string(),
        }
    ));
    out.push_str(&format!(
        "  eta        {}\n",
        match view.eta_secs {
            Some(seconds) => format!("{seconds}s"),
            None => "unknown".to_string(),
        }
    ));
    out.push_str(&format!(
        "  segments   {} (source honours ranges: {})\n",
        view.segments.len(),
        if view.range_supported { "yes" } else { "no" }
    ));
    for segment in &view.segments {
        // Clippy's `uninlined_format_args` wants the last one inline, but these
        // are positional on purpose: the numbers read better than the names.
        #[allow(clippy::uninlined_format_args)]
        {
            out.push_str(&format!(
                "    #{:<3} [{}, {}]  {} durable\n",
                segment.id,
                segment.start,
                segment.end,
                bytes(segment.done_len)
            ));
        }
    }
    if let Some(failure) = &view.failure {
        out.push_str(&format!("  failed     {}\n", failure_line(failure)));
    }
    out
}

/// A structured failure as one line, with the engine's own tokens kept
/// alongside it.
pub fn failure(failure: &Failure) -> String {
    let description = match failure.kind {
        FailureKind::SourceMissing => {
            "the source does not exist (bad address, or the resource is gone)"
        }
        FailureKind::AccessDenied => "the source refused access (authentication failed)",
        FailureKind::RateLimited => "the source is throttling us; it needs to be retried later",
        FailureKind::CredentialExpired => "the address has expired and has to be re-acquired",
        FailureKind::TransportReset => "the network dropped (connection reset or timed out)",
        FailureKind::ServerError => "the source server reported an error",
        FailureKind::DiskFull => "out of disk space",
        FailureKind::DiskOther => "the local write failed (permissions or an illegal path)",
        FailureKind::DataMismatch => {
            "the source returned data that does not match the request (nothing was written)"
        }
        FailureKind::ChecksumMismatch => "the checksum does not match",
        FailureKind::UserCancelled => "cancelled",
        FailureKind::Unknown => "unknown error",
    };
    let mut line = format!("{description} [{}]", failure.kind.as_str());
    if let Some(status) = failure.status {
        line.push_str(&format!(" (HTTP {status})"));
    }
    line.push_str(&format!(" {}", failure.detail));
    line
}

/// The same thing with the scope on the front, for the `status` block.
fn failure_line(reason: &Failure) -> String {
    format!("{} - {}", reason.scope.as_str(), failure(reason))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_stay_short() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(8 * 1024 * 1024), "8.0 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn every_rendered_line_is_ascii() {
        // The acceptance suite decodes this process's output with the platform
        // locale codec; a stray non-ASCII byte aborts the suite.
        for kind in [
            FailureKind::SourceMissing,
            FailureKind::AccessDenied,
            FailureKind::RateLimited,
            FailureKind::CredentialExpired,
            FailureKind::TransportReset,
            FailureKind::ServerError,
            FailureKind::DiskFull,
            FailureKind::DiskOther,
            FailureKind::DataMismatch,
            FailureKind::ChecksumMismatch,
            FailureKind::UserCancelled,
            FailureKind::Unknown,
        ] {
            let reason = Failure::new(kind, chua_engine::FailureScope::Transfer, "detail");
            let line = failure(&reason);
            assert!(line.is_ascii(), "not ASCII: {line}");
            assert!(bytes(u64::MAX).is_ascii());
        }
    }
}
