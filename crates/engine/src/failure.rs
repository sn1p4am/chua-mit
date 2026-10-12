//! Failure classification and backoff policy.
//!
//! The engine never produces user-facing text. Every failure is reported as a
//! structured [`Failure`] that answers the two questions the host needs:
//! *which class* and *is it retryable*. The host owns wording.
//!
//! See `spec/engine-v1.md` §8.

use std::fmt;
use std::time::Duration;

use crate::state::TransitionError;

/// Which stage of the pipeline a failure came from. Diagnostic only — the host
/// may ignore it; classification and retryability are carried by [`Failure::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    /// Metadata probe.
    Probe,
    /// Segment body transfer.
    Transfer,
    /// Local file or database work.
    Storage,
    /// Post-download checksum.
    Verification,
    /// Scheduler / model level.
    Model,
}

impl FailureScope {
    /// Stable identifier, suitable for logs and host-side lookup tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Transfer => "transfer",
            Self::Storage => "storage",
            Self::Verification => "verification",
            Self::Model => "model",
        }
    }
}

/// The twelve failure classes of `spec/engine-v1.md` §8.2.
///
/// Every variant has an explicit retryability answer, see
/// [`FailureKind::retryable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The resource is gone.
    SourceMissing,
    /// Authentication or authorisation was refused.
    AccessDenied,
    /// The server is throttling us.
    RateLimited,
    /// A time-limited URL went stale; the address has to be re-acquired rather
    /// than retried as-is.
    CredentialExpired,
    /// Connection reset, timeout, temporarily unreachable.
    TransportReset,
    /// The remote answered 5xx.
    ServerError,
    /// Out of space; needs a human.
    DiskFull,
    /// Any other local storage error: permissions, illegal path.
    DiskOther,
    /// The bytes the server produced do not match what was asked for, or the
    /// resource changed shape underneath us.
    DataMismatch,
    /// A checksum did not match. Retried once, then fatal.
    ChecksumMismatch,
    /// An explicit cancellation.
    UserCancelled,
    /// Anything not covered above. Retried a bounded number of times.
    Unknown,
}

impl FailureKind {
    /// Whether the engine may retry this class on its own.
    ///
    /// `CredentialExpired` and `ChecksumMismatch` are retryable in a qualified
    /// way that the transfer loop enforces: the former must go through the
    /// address re-acquisition path, the latter gets exactly one retry.
    pub const fn retryable(self) -> bool {
        match self {
            Self::SourceMissing
            | Self::AccessDenied
            | Self::DiskFull
            | Self::DiskOther
            | Self::UserCancelled => false,
            Self::RateLimited
            | Self::CredentialExpired
            | Self::TransportReset
            | Self::ServerError
            | Self::DataMismatch
            | Self::ChecksumMismatch
            | Self::Unknown => true,
        }
    }

    /// Retryable classes that are dangerous to retry as-is are marked here so
    /// the transfer loop can route them differently.
    pub const fn needs_fresh_address(self) -> bool {
        matches!(self, Self::CredentialExpired)
    }

    /// Stable identifier, suitable for logs and host-side lookup tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceMissing => "source_missing",
            Self::AccessDenied => "access_denied",
            Self::RateLimited => "rate_limited",
            Self::CredentialExpired => "credential_expired",
            Self::TransportReset => "transport_reset",
            Self::ServerError => "server_error",
            Self::DiskFull => "disk_full",
            Self::DiskOther => "disk_other",
            Self::DataMismatch => "data_mismatch",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::UserCancelled => "user_cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// A structured failure: class, retryability, and machine-readable context.
///
/// No field carries prose. `detail` is a short machine-oriented token such as
/// `"range_mismatch"` or `"http_404"`; hosts are free to ignore it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Failure {
    /// Which class this failure belongs to.
    pub kind: FailureKind,
    /// Whether a retry is permitted by policy.
    pub retryable: bool,
    /// Where it happened.
    pub scope: FailureScope,
    /// HTTP status, when the failure came from a response.
    pub status: Option<u16>,
    /// Server-supplied wait, from the throttling wait header.
    pub retry_after: Option<Duration>,
    /// Short machine-readable token, never a sentence.
    pub detail: String,
}

impl Failure {
    /// Build a failure, deriving retryability from the class.
    pub fn new(kind: FailureKind, scope: FailureScope, detail: impl Into<String>) -> Self {
        Self {
            kind,
            retryable: kind.retryable(),
            scope,
            status: None,
            retry_after: None,
            detail: detail.into(),
        }
    }

    /// Attach the HTTP status that produced this failure.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Attach a server-supplied retry delay (throttling responses).
    pub fn with_retry_after(mut self, after: Option<Duration>) -> Self {
        self.retry_after = after;
        self
    }

    /// Narrow the retryability flag, keeping the class. Used where one class is
    /// retryable in general but a specific occurrence is not.
    pub fn not_retryable(mut self) -> Self {
        self.retryable = false;
        self
    }

    /// The class of a task-level transition rejection.
    pub fn from_transition(error: &TransitionError) -> Self {
        Self::new(
            FailureKind::Unknown,
            FailureScope::Model,
            format!("illegal_transition:{}", error.tag()),
        )
        .not_retryable()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}:{}",
            self.scope.as_str(),
            self.kind.as_str(),
            self.detail
        )
    }
}

/// Exponential backoff with jitter, bounded in both attempts and interval.
///
/// See `spec/engine-v1.md` §8.3. Throttling responses override the curve with
/// the server-supplied delay; see [`BackoffPolicy::next_delay`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackoffPolicy {
    /// Total attempts allowed, including the first one. Must be >= 1.
    pub max_attempts: u32,
    /// Delay before the second attempt.
    pub initial: Duration,
    /// Upper bound on any single delay.
    pub max_delay: Duration,
    /// Jitter is applied as a fraction of the computed delay, in `[0.0, 1.0)`.
    pub jitter_ratio: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial: Duration::from_millis(500),
            max_delay: Duration::from_secs(60),
            jitter_ratio: 0.25,
        }
    }
}

impl BackoffPolicy {
    /// Delay before `attempt` (1-based: `attempt == 1` is the first try and has
    /// no preceding delay).
    ///
    /// `attempt` counts the attempt that just failed; the returned delay is how
    /// long to wait before the next one. Returns `None` once the budget is
    /// exhausted, which is the signal to move the task to `Failed`.
    ///
    /// `jitter` must be a deterministic sample in `[0.0, 1.0)`; callers pass a
    /// fresh random value so parallel tasks do not retry in lockstep.
    pub fn next_delay(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        jitter: f64,
    ) -> Option<Duration> {
        if attempt == 0 || attempt >= self.max_attempts {
            return None;
        }
        let exponential = self
            .initial
            .saturating_mul(1u32 << (attempt - 1).min(16))
            .min(self.max_delay);
        let base = match retry_after {
            // A throttling response wins over our own curve (§8.3), but only
            // within the interval ceiling the policy sets.
            Some(server) => server.clamp(self.initial, self.max_delay),
            None => exponential,
        };
        let spread = base.mul_f64(self.jitter_ratio.clamp(0.0, 1.0));
        let factor = jitter.clamp(0.0, 1.0);
        let jittered = base.saturating_sub(spread) + spread.mul_f64(factor);
        Some(jittered.min(self.max_delay))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_class_answers_retryable() {
        // The twelve classes of §8.2, each with its explicit answer.
        let expected = [
            (FailureKind::SourceMissing, false),
            (FailureKind::AccessDenied, false),
            (FailureKind::RateLimited, true),
            (FailureKind::CredentialExpired, true),
            (FailureKind::TransportReset, true),
            (FailureKind::ServerError, true),
            (FailureKind::DiskFull, false),
            (FailureKind::DiskOther, false),
            (FailureKind::DataMismatch, true),
            (FailureKind::ChecksumMismatch, true),
            (FailureKind::UserCancelled, false),
            (FailureKind::Unknown, true),
        ];
        assert_eq!(expected.len(), 12);
        for (kind, retryable) in expected {
            assert_eq!(kind.retryable(), retryable, "class {}", kind.as_str());
            let failure = Failure::new(kind, FailureScope::Transfer, "x");
            assert_eq!(failure.retryable, retryable);
        }
    }

    #[test]
    fn transition_failure_is_not_retryable() {
        let error = TransitionError::Illegal {
            from: crate::state::TaskState::Completed,
            to: crate::state::TaskState::Downloading,
        };
        let failure = Failure::from_transition(&error);
        assert!(!failure.retryable);
        assert_eq!(failure.scope, FailureScope::Model);
    }

    #[test]
    fn backoff_grows_and_then_stops() {
        let policy = BackoffPolicy::default();
        let mut previous = Duration::ZERO;
        for attempt in 1..policy.max_attempts {
            let delay = policy.next_delay(attempt, None, 0.5).unwrap();
            assert!(delay > previous, "attempt {attempt}");
            previous = delay;
        }
        assert!(policy.next_delay(policy.max_attempts, None, 0.5).is_none());
        assert!(policy.next_delay(0, None, 0.5).is_none());
    }

    #[test]
    fn backoff_is_capped() {
        let policy = BackoffPolicy {
            max_attempts: 40,
            initial: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            jitter_ratio: 0.0,
        };
        for attempt in 1..policy.max_attempts {
            let delay = policy.next_delay(attempt, None, 0.0).unwrap();
            assert!(delay <= policy.max_delay);
        }
    }

    #[test]
    fn throttling_delay_overrides_the_curve() {
        // Jitter off, so the comparison is exact rather than a band.
        let policy = BackoffPolicy {
            jitter_ratio: 0.0,
            ..BackoffPolicy::default()
        };
        // Seven seconds is inside the policy's band, so the server's number wins
        // outright.
        let server = Some(Duration::from_secs(7));
        assert_eq!(
            policy.next_delay(1, server, 0.0).unwrap(),
            Duration::from_secs(7)
        );
        // Beyond the ceiling, the ceiling applies: §8.3 requires the maximum
        // interval to be bounded whatever the server asks for.
        let huge = Some(Duration::from_secs(600));
        assert_eq!(policy.next_delay(1, huge, 0.0).unwrap(), policy.max_delay);
        // Below the floor, the floor applies, so a hostile `Retry-After: 0`
        // cannot turn into a retry storm.
        let zero = Some(Duration::ZERO);
        assert_eq!(policy.next_delay(1, zero, 0.0).unwrap(), policy.initial);
    }

    #[test]
    fn jitter_stays_within_the_band() {
        let policy = BackoffPolicy::default();
        let low = policy.next_delay(3, None, 0.0).unwrap();
        let high = policy.next_delay(3, None, 1.0).unwrap();
        assert!(low < high);
    }
}
