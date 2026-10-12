//! Engine configuration, and the segment-layout rules of `spec/engine-v1.md`
//! §6.1 and §11.
//!
//! Configuration is supplied by the host; the engine never reads it from a
//! particular file or registry (§11, closing note).

use std::time::Duration;

use crate::failure::{BackoffPolicy, Failure, FailureKind, FailureScope};

/// Absolute ceiling on the per-task segment count.
///
/// A configured value above this is rejected rather than silently truncated
/// (§6.1). The number is a memory-and-socket budget: each live segment holds one
/// connection and one buffer.
pub const HARD_MAX_SEGMENTS: u32 = 64;

/// Floor on a single segment's size: below this the scheduling overhead costs
/// more than the parallelism gains (§6.1).
pub const MIN_SEGMENT_BYTES: u64 = 1024 * 1024;

/// Files at or below this size use one segment (§6.1).
pub const SINGLE_SEGMENT_UP_TO: u64 = 4 * 1024 * 1024;

/// Segment layout rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentPolicy {
    /// Maximum segments per task. Must be in `1..=HARD_MAX_SEGMENTS`.
    pub max_segments: u32,
    /// Boundaries are aligned down to a multiple of this, so that splits and
    /// re-plans do not litter the file with tiny slivers.
    pub granularity: u64,
    /// Below this size a file is fetched in one segment.
    pub single_segment_up_to: u64,
    /// Floor on one segment's size.
    pub min_segment_bytes: u64,
}

impl Default for SegmentPolicy {
    fn default() -> Self {
        Self {
            max_segments: 8,
            granularity: MIN_SEGMENT_BYTES,
            single_segment_up_to: SINGLE_SEGMENT_UP_TO,
            min_segment_bytes: MIN_SEGMENT_BYTES,
        }
    }
}

impl SegmentPolicy {
    /// Check the policy against the hard limits of §6.1.
    pub fn validate(&self) -> Result<(), Failure> {
        if self.max_segments == 0 {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "max_segments_zero",
            )
            .not_retryable());
        }
        if self.max_segments > HARD_MAX_SEGMENTS {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                format!("max_segments_above_hard_limit_{HARD_MAX_SEGMENTS}"),
            )
            .not_retryable());
        }
        if self.granularity == 0 || self.min_segment_bytes == 0 {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "segment_granularity_zero",
            )
            .not_retryable());
        }
        Ok(())
    }

    /// How many segments a file of `total_len` should use.
    ///
    /// Small files get one segment; larger ones get as many as fit while keeping
    /// every segment at or above the floor, capped by `max_segments`.
    pub fn segment_count_for(&self, total_len: u64, range_supported: bool) -> u32 {
        if !range_supported || total_len <= self.single_segment_up_to {
            return 1;
        }
        let by_floor = (total_len / self.min_segment_bytes).max(1);
        let count = by_floor.min(u64::from(self.max_segments)).max(1);
        // `count` is bounded by `max_segments <= HARD_MAX_SEGMENTS`, so the cast
        // cannot lose information.
        count as u32
    }

    /// Split `[0, total_len)` into `count` intervals that exactly tile it.
    ///
    /// Boundaries are aligned to `granularity` except for the final segment,
    /// which always ends at `total_len - 1`. The result satisfies invariant I1:
    /// the union is the whole range and no two intervals overlap.
    pub fn plan(&self, total_len: u64, range_supported: bool) -> Vec<(u64, u64)> {
        if total_len == 0 {
            return Vec::new();
        }
        let count = self.segment_count_for(total_len, range_supported);
        if count <= 1 {
            return vec![(0, total_len - 1)];
        }
        let mut boundaries = Vec::with_capacity(count as usize + 1);
        boundaries.push(0u64);
        for index in 1..count {
            let raw = total_len.saturating_mul(u64::from(index)) / u64::from(count);
            let aligned = (raw / self.granularity) * self.granularity;
            // Never collapse onto the previous boundary, and always leave room
            // for the remaining segments.
            let floor = boundaries[boundaries.len() - 1] + 1;
            let remaining = u64::from(count - index);
            let ceiling = total_len.saturating_sub(remaining);
            boundaries.push(aligned.clamp(floor, ceiling.max(floor)));
        }
        boundaries.push(total_len);
        boundaries
            .windows(2)
            .filter_map(|pair| match pair {
                [start, end] if end > start => Some((*start, end - 1)),
                _ => None,
            })
            .collect()
    }
}

/// Engine-wide configuration (§11).
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// How many tasks may be `Downloading` at once.
    pub global_concurrency: usize,
    /// Segment layout rules.
    pub segment_policy: SegmentPolicy,
    /// Global transfer ceiling in bytes per second; `None` is unlimited.
    pub global_rate_limit_bps: Option<u64>,
    /// How often segment progress is written to durable storage (§7.2).
    pub progress_flush_interval: Duration,
    /// Upper bound on progress event emission, in hertz (§9.4).
    pub event_hz: u32,
    /// Sliding window used for rate and ETA, in seconds (§9.2).
    pub rate_window_secs: u64,
    /// Retry policy (§8.3).
    pub backoff: BackoffPolicy,
    /// Total time allowed for one probe attempt (§4.4).
    pub probe_timeout: Duration,
    /// Whether a completed transfer is verified (§7.5).
    pub verify: bool,
    /// Whether non-terminal tasks found at startup are resumed automatically
    /// (§7.3).
    pub auto_resume: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            global_concurrency: 3,
            segment_policy: SegmentPolicy::default(),
            global_rate_limit_bps: None,
            progress_flush_interval: Duration::from_secs(3),
            event_hz: 10,
            rate_window_secs: 5,
            backoff: BackoffPolicy::default(),
            probe_timeout: Duration::from_secs(30),
            verify: false,
            auto_resume: true,
        }
    }
}

impl EngineConfig {
    /// Reject configurations that cannot be honoured.
    pub fn validate(&self) -> Result<(), Failure> {
        self.segment_policy.validate()?;
        if self.global_concurrency == 0 {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "global_concurrency_zero",
            )
            .not_retryable());
        }
        if self.event_hz == 0 || self.event_hz > 1000 {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "event_hz_out_of_range",
            )
            .not_retryable());
        }
        if self.rate_window_secs == 0 {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "rate_window_zero",
            )
            .not_retryable());
        }
        if self.progress_flush_interval.is_zero() {
            return Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                "flush_interval_zero",
            )
            .not_retryable());
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_spec_suggestions() {
        let config = EngineConfig::default();
        assert_eq!(config.global_concurrency, 3);
        assert_eq!(config.segment_policy.max_segments, 8);
        assert_eq!(config.segment_policy.granularity, 4 * 1024 * 1024);
        assert_eq!(config.event_hz, 10);
        assert_eq!(config.rate_window_secs, 5);
        assert!(config.progress_flush_interval >= Duration::from_secs(2));
        assert!(config.progress_flush_interval <= Duration::from_secs(5));
        assert_eq!(config.backoff.max_attempts, 5);
        assert_eq!(config.backoff.max_delay, Duration::from_secs(60));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_oversized_segment_count_is_rejected_not_clamped() {
        let config = EngineConfig {
            segment_policy: SegmentPolicy {
                max_segments: HARD_MAX_SEGMENTS + 1,
                ..SegmentPolicy::default()
            },
            ..EngineConfig::default()
        };
        let failure = config.validate().unwrap_err();
        assert!(!failure.retryable);
        assert_eq!(failure.detail, "max_segments_above_hard_limit_64");
    }

    #[test]
    fn zeroed_fields_are_rejected() {
        for config in [
            EngineConfig {
                global_concurrency: 0,
                ..EngineConfig::default()
            },
            EngineConfig {
                event_hz: 0,
                ..EngineConfig::default()
            },
            EngineConfig {
                rate_window_secs: 0,
                ..EngineConfig::default()
            },
            EngineConfig {
                progress_flush_interval: Duration::ZERO,
                ..EngineConfig::default()
            },
            EngineConfig {
                segment_policy: SegmentPolicy {
                    max_segments: 0,
                    ..SegmentPolicy::default()
                },
                ..EngineConfig::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn small_files_use_one_segment() {
        let policy = SegmentPolicy::default();
        assert_eq!(policy.plan(0, true), Vec::new());
        assert_eq!(policy.plan(1, true), vec![(0, 0)]);
        assert_eq!(policy.plan(1024, true), vec![(0, 1023)]);
        assert_eq!(
            policy.plan(SINGLE_SEGMENT_UP_TO, true),
            vec![(0, SINGLE_SEGMENT_UP_TO - 1)]
        );
        // Without range support the count is one whatever the size.
        assert_eq!(policy.plan(100 * 1024 * 1024, false), vec![(0, 100 * 1024 * 1024 - 1)]);
    }

    #[test]
    fn large_files_are_split_and_still_tile_exactly() {
        let policy = SegmentPolicy::default();
        for total in [
            8 * 1024 * 1024u64,
            100 * 1024 * 1024,
            1024 * 1024 * 1024,
            3_000_000_007,
        ] {
            let plan = policy.plan(total, true);
            assert!(plan.len() > 1, "total {total}");
            assert!(plan.len() <= policy.max_segments as usize);
            assert_eq!(plan[0].0, 0);
            assert_eq!(plan[plan.len() - 1].1, total - 1);
            let mut next = 0u64;
            for (start, end) in &plan {
                assert_eq!(*start, next, "gap before {start} in {plan:?}");
                assert!(end >= start);
                assert!(*end - *start + 1 >= 1);
                next = end + 1;
            }
            assert_eq!(next, total);
        }
    }

    #[test]
    fn segment_count_respects_the_floor_and_the_cap() {
        let policy = SegmentPolicy {
            max_segments: 8,
            granularity: 4 * 1024 * 1024,
            single_segment_up_to: 4 * 1024 * 1024,
            min_segment_bytes: 4 * 1024 * 1024,
        };
        // 8 MiB with a 4 MiB floor: two segments.
        assert_eq!(policy.segment_count_for(8 * 1024 * 1024, true), 2);
        // 1 GiB with a 4 MiB floor would be 256, capped at 8.
        assert_eq!(policy.segment_count_for(1024 * 1024 * 1024, true), 8);
        // Just above the single-segment threshold: still one, because one
        // segment would already be above the floor but two would not.
        assert_eq!(policy.segment_count_for(5 * 1024 * 1024, true), 1);
    }

    #[test]
    fn alignment_keeps_boundaries_on_the_granularity() {
        let policy = SegmentPolicy {
            max_segments: 4,
            granularity: 1024,
            single_segment_up_to: 0,
            min_segment_bytes: 1,
        };
        let plan = policy.plan(10_000, true);
        for (start, _end) in plan.iter().skip(1) {
            assert_eq!(start % 1024, 0, "boundary {start} is not aligned");
        }
    }

    #[test]
    fn plan_covers_pathological_sizes() {
        let policy = SegmentPolicy {
            max_segments: 8,
            granularity: 4 * 1024 * 1024,
            single_segment_up_to: 0,
            min_segment_bytes: 1,
        };
        // Bigger than the granularity but with far fewer bytes than the cap
        // would like per segment.
        for total in [2u64, 3, 17, 4097, 4 * 1024 * 1024 + 1] {
            let plan = policy.plan(total, true);
            let covered: u64 = plan.iter().map(|(s, e)| e - s + 1).sum();
            assert_eq!(covered, total, "total {total} plan {plan:?}");
            assert_eq!(plan[plan.len() - 1].1, total - 1);
        }
    }
}
