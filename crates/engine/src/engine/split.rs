//! The `spec/engine-v1.md` §6.2 dynamic-adjustment decision, as pure functions.
//!
//! §6.2 asks for three things and forbids a fourth:
//!
//! * a segment that has fallen behind SHOULD be split in two and both halves
//!   fetched in parallel;
//! * splitting MUST preserve invariant I1 — the intervals tile `[0, total_len)`
//!   with no overlap;
//! * a completed segment MUST NOT be split;
//! * splitting SHOULD have a cooldown so it does not thrash.
//!
//! Deciding is separated from doing on purpose. This module never touches a
//! `ProgressBoard`, a file or a socket, so every rule above is testable as an
//! arithmetic statement about intervals. The mutation itself lives in
//! [`crate::engine::transfer`], where it can be made atomic with respect to a
//! concurrent chunk write.
//!
//! # Where the split point is
//!
//! The split point is the segment's **durable frontier**, not its midpoint. That
//! is what makes the two halves carry the right progress without any byte
//! changing hands:
//!
//! * the first half becomes `[start, frontier - 1]`, which is exactly the bytes
//!   already confirmed durable — so it is complete the moment it is created, and
//!   nothing is re-fetched and nothing is lost;
//! * the second half becomes `[frontier, end]` and starts empty;
//! * together they are exactly the interval that was there before, so I1 holds
//!   before, during and after.
//!
//! A midpoint split would have to move already-confirmed bytes from one segment
//! to the other, and there is no way to do that which is not observable as a
//! byte belonging to two intervals at once.
//!
//! The cost of that choice is a bias: the half that keeps going is the one with
//! the most bytes left. That is the right bias — the segment being split is by
//! definition the one that needs the most help — and it is why the split is only
//! offered when both halves clear the §6.1 floor.

use std::time::Duration;

use crate::config::HARD_MAX_SEGMENTS;
use crate::model::SegmentState;

/// Splitting policy (`spec/engine-v1.md` §6.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitPolicy {
    /// Whether a lagging segment may be split at all.
    pub enabled: bool,
    /// How often the scheduler looks for a lagging segment.
    pub check_interval: Duration,
    /// Shortest gap between two splits of one task. §6.2's anti-thrash rule.
    ///
    /// It is measured from the *last split of this task*, not from the last
    /// check: a task that keeps producing laggards should still ramp, just not
    /// faster than this.
    pub cooldown: Duration,
    /// How long a segment may make no progress at all before it is a laggard.
    ///
    /// A segment whose lane is waiting on a socket that will never answer is
    /// the case this catches, and it is the one where a split pays for itself
    /// immediately: the replacement request goes out on a fresh connection.
    pub stall: Duration,
    /// How far behind the task's mean progress a segment must be, as a fraction
    /// of its own length, to count as lagging. `0.5` means half the file.
    pub lag_ratio: f64,
    /// Smallest share of a task's remaining bytes a segment must own before its
    /// lag is worth acting on. `0.05` is one twentieth.
    pub lag_share: f64,
}

impl Default for SplitPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval: Duration::from_millis(500),
            cooldown: Duration::from_secs(5),
            stall: Duration::from_secs(5),
            lag_ratio: 0.5,
            lag_share: 0.05,
        }
    }
}

impl SplitPolicy {
    /// Reject values that would make the policy meaningless or degenerate.
    pub fn validate(&self) -> Result<(), crate::failure::Failure> {
        use crate::failure::{Failure, FailureKind, FailureScope};
        let reject = |detail: &str| {
            Err(Failure::new(
                FailureKind::Unknown,
                FailureScope::Model,
                detail,
            )
            .not_retryable())
        };
        if self.check_interval.is_zero() {
            return reject("split_check_interval_zero");
        }
        // A cooldown of zero is "no cooldown", which is a legitimate choice, but
        // §6.2's anti-thrash rule means it has to be an explicit one. Only the
        // pathological values are refused: a ratio outside `(0, 1]` can never be
        // met, so a task configured that way would silently never split.
        if !(self.lag_ratio > 0.0 && self.lag_ratio <= 1.0) {
            return reject("split_lag_ratio_out_of_range");
        }
        if !(self.lag_share >= 0.0 && self.lag_share < 1.0) {
            return reject("split_lag_share_out_of_range");
        }
        Ok(())
    }
}

/// How much of a segment is durably on disk.
///
/// The scheduler reads this from the progress board, which is the only place a
/// concurrent chunk write can move it. Testing takes the same route rather than
/// working out the answer from the intervals.
pub trait SplitDensity {
    /// Durable bytes recorded for `id`; `0` when the segment is not known.
    fn durable_bytes(&self, id: u32) -> u64;
}

/// Why a segment was picked to be split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitReason {
    /// The segment's lane has made no progress at all for `policy.stall`.
    Stalled,
    /// The segment is progressing, but is far enough behind the task's mean
    /// progress and owns enough of the remaining bytes to be worth halving.
    Behind,
}

impl SplitReason {
    /// Stable identifier, for the `SegmentsChanged` detail.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stalled => "stalled",
            Self::Behind => "behind",
        }
    }
}

/// The split the scheduler should perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    /// Identity of the segment being split.
    pub id: u32,
    /// Id the first half takes. The second half keeps [`SplitPlan::id`], so the
    /// lane that is already fetching that interval keeps its identity and its
    /// in-flight accounting.
    pub front_id: u32,
    /// First byte of the first half (the interval's own start).
    pub front_start: u64,
    /// Last byte of the first half: the durable frontier.
    pub front_end: u64,
    /// First byte of the second half.
    pub back_start: u64,
    /// Last byte of the second half (the interval's own end).
    pub back_end: u64,
    /// Why this segment was chosen.
    pub reason: SplitReason,
}

/// Pick one segment to split, if any is eligible.
///
/// `within_cooldown` short-circuits everything else: §6.2's cooldown is about
/// how often a *task* may split, so a fresh laggard found one tick after a split
/// waits its turn.
///
/// The count of segments is read from `segments` itself, so the cap can never be
/// checked against a number that disagrees with the layout being handed in.
#[allow(clippy::too_many_arguments)]
pub fn plan_split(
    segments: &[SegmentState],
    density: &dyn SplitDensity,
    behind: &dyn Fn(u32) -> bool,
    stalled: &dyn Fn(u32) -> bool,
    policy: &SplitPolicy,
    min_segment_bytes: u64,
    max_segments: u32,
    within_cooldown: bool,
) -> Option<SplitPlan> {
    if !policy.enabled || within_cooldown || segments.is_empty() {
        return None;
    }
    // §6.1's hard ceiling is absolute and §6.2 may not find a way around it. A
    // task already at its cap simply does not split, which is a decision and not
    // an error: running at the maximum parallelism is the intended end state.
    let cap = max_segments.min(HARD_MAX_SEGMENTS);
    let count = u32::try_from(segments.len()).unwrap_or(u32::MAX);
    if count >= cap {
        return None;
    }

    // A tie is broken by the lowest id, so the choice does not depend on the
    // order the board happens to report. Deterministic choices are what make
    // this testable.
    let mut best: Option<(u32, u64)> = None;
    for segment in segments {
        let durable = density.durable_bytes(segment.id);
        if split_point(
            segment,
            durable,
            behind(segment.id),
            stalled(segment.id),
            min_segment_bytes,
        )
        .is_none()
        {
            continue;
        }
        let remaining = segment.len().saturating_sub(durable);
        let better = match best {
            None => true,
            Some((_, best_remaining)) => remaining > best_remaining,
        };
        if better {
            best = Some((segment.id, remaining));
        }
    }
    let (id, _) = best?;
    let segment = segments.iter().find(|candidate| candidate.id == id)?;
    let durable = density.durable_bytes(id);
    let (front_end, reason) = split_point(
        segment,
        durable,
        behind(id),
        stalled(id),
        min_segment_bytes,
    )?;
    Some(SplitPlan {
        id,
        front_id: next_free_id(segments),
        front_start: segment.start,
        front_end,
        back_start: front_end.saturating_add(1),
        back_end: segment.end,
        reason,
    })
}

/// Whether one segment may be halved, and where.
///
/// Returns `None` for every case §6.2 forbids or that §6.1's floor rules out:
/// a completed segment, a segment whose durable frontier is not yet past its own
/// start (nothing to hand to the first half), and a segment that would leave a
/// half below the floor.
fn split_point(
    segment: &SegmentState,
    durable: u64,
    behind: bool,
    stalled: bool,
    min_segment_bytes: u64,
) -> Option<(u64, SplitReason)> {
    // §6.2: a completed segment MUST NOT be split. Checked first, because a
    // completed segment is the one case where a "frontier" reading is
    // meaningless: its whole interval is durable, so there is nothing left to
    // hand to a second half.
    if segment.is_complete() || segment.is_empty() {
        return None;
    }
    // The first half is exactly the durable prefix, so it exists only once at
    // least one byte has been confirmed.
    if durable == 0 {
        return None;
    }
    let front_end = segment.start.saturating_add(durable).saturating_sub(1);
    if front_end < segment.start || front_end >= segment.end {
        return None;
    }
    // Both halves have to clear the §6.1 floor. The floor is a MUST there and
    // §6.2 gives no licence to break it, so a segment that cannot be halved
    // without producing a sliver is left alone.
    let front_len = front_end.saturating_sub(segment.start).saturating_add(1);
    let back_len = segment.end.saturating_sub(front_end);
    if front_len < min_segment_bytes || back_len < min_segment_bytes {
        return None;
    }
    let reason = if stalled {
        SplitReason::Stalled
    } else if behind {
        SplitReason::Behind
    } else {
        return None;
    };
    Some((front_end, reason))
}

/// An id no segment in `segments` is using.
fn next_free_id(segments: &[SegmentState]) -> u32 {
    segments
        .iter()
        .map(|segment| segment.id)
        .max()
        .map_or(0, |highest| highest.saturating_add(1))
}

/// Tracks how each segment's lane is getting on, so the scheduler can tell a
/// stalled lane from a busy one.
///
/// The board's durable count is the wrong signal for this: it only moves when a
/// flush confirms bytes, which happens every `progress_flush_interval`. What a
/// laggard detector needs is the frontier a lane has actually written to, and
/// the board keeps that too, in `received_len`.
#[derive(Debug, Default)]
pub struct LagTracker {
    entries: Vec<Tracked>,
}

#[derive(Debug)]
struct Tracked {
    id: u32,
    received: u64,
    at: tokio::time::Instant,
}

impl LagTracker {
    /// A tracker with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything. Used when the layout is replaced wholesale.
    pub fn reset(&mut self) {
        self.entries.clear();
    }

    /// Record the observed frontier of every segment.
    ///
    /// `received` is the segment's durable plus pending bytes, which is the
    /// number of bytes of that interval that are on disk. A segment whose value
    /// has not moved since the previous tick keeps its earlier timestamp, which
    /// is what makes the stall test measure a *duration of no progress* rather
    /// than a duration since the tick.
    pub fn observe(&mut self, received: &dyn Fn(u32) -> Option<u64>, now: tokio::time::Instant) {
        for entry in &mut self.entries {
            if let Some(current) = received(entry.id)
                && current > entry.received
            {
                entry.received = current;
                entry.at = now;
            }
        }
    }

    /// Add a segment to the tracker if it is not already there.
    pub fn register(&mut self, id: u32, received: u64, now: tokio::time::Instant) {
        if self.entries.iter().any(|entry| entry.id == id) {
            return;
        }
        self.entries.push(Tracked {
            id,
            received,
            at: now,
        });
    }

    /// Whether a segment has stood still for at least `stall`.
    pub fn has_stalled(&self, id: u32, now: tokio::time::Instant, stall: Duration) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .is_some_and(|entry| now.duration_since(entry.at) >= stall)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A density map that answers from a fixed table.
    struct Fixed(Vec<(u32, u64)>);

    impl SplitDensity for Fixed {
        fn durable_bytes(&self, id: u32) -> u64 {
            self.0
                .iter()
                .find(|(candidate, _)| *candidate == id)
                .map_or(0, |(_, bytes)| *bytes)
        }
    }

    fn policy() -> SplitPolicy {
        // A one-second stall and no cooldown, so the tests read as arithmetic
        // about intervals rather than about time.
        SplitPolicy {
            enabled: true,
            check_interval: Duration::from_millis(50),
            cooldown: Duration::ZERO,
            stall: Duration::from_secs(1),
            lag_ratio: 0.5,
            lag_share: 0.05,
        }
    }

    const FLOOR: u64 = 1024;

    #[allow(clippy::too_many_arguments)]
    fn plan(
        segments: &[SegmentState],
        durable: &[(u32, u64)],
        stalled: &[u32],
        behind: bool,
        max_segments: u32,
    ) -> Option<SplitPlan> {
        let density = Fixed(durable.to_vec());
        plan_split(
            segments,
            &density,
            &|_| behind,
            &|id| stalled.contains(&id),
            &policy(),
            FLOOR,
            max_segments,
            false,
        )
    }

    #[test]
    fn a_stalled_segment_is_split_at_its_durable_frontier() {
        // Something like two minutes of progress, then the lane goes quiet.
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(2047);
        let result = plan(&[segment], &[(0, 2048)], &[0], false, 4).unwrap();
        assert_eq!(result.reason, SplitReason::Stalled);
        assert_eq!(result.id, 0);
        // The front half is exactly the confirmed bytes; the back half starts at
        // the next byte. Neither is re-fetched, and neither is lost.
        assert_eq!(result.front_start, 0);
        assert_eq!(result.front_end, 2047);
        assert_eq!(result.back_start, 2048);
        assert_eq!(result.back_end, 8191);
    }

    #[test]
    fn the_two_halves_tile_the_original_interval_exactly() {
        // The I1 check, as arithmetic: no overlap, no gap, same total.
        let mut segment = SegmentState::new(3, 1000, 9999);
        segment.mark_durable_through(4999);
        let result = plan(&[segment.clone()], &[(3, 4000)], &[3], false, 8).unwrap();
        assert_eq!(result.back_start, result.front_end + 1);
        assert_eq!(result.front_start, segment.start);
        assert_eq!(result.back_end, segment.end);
        let covered =
            (result.front_end - result.front_start + 1) + (result.back_end - result.back_start + 1);
        assert_eq!(covered, segment.len());
    }

    #[test]
    fn the_first_half_carries_all_of_the_durable_progress() {
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(4095);
        let result = plan(&[segment], &[(0, 4096)], &[0], false, 4).unwrap();
        // The front half's interval is the durable prefix, which is what makes
        // it complete on creation.
        assert_eq!(result.front_end - result.front_start + 1, 4096);
        let front = SegmentState {
            id: result.front_id,
            start: result.front_start,
            end: result.front_end,
            durable_end: Some(result.front_end),
        };
        assert!(front.is_complete());
        // ... and the second half starts empty.
        let back = SegmentState::new(result.id, result.back_start, result.back_end);
        assert_eq!(back.durable_len(), 0);
        assert!(!back.is_complete());
    }

    #[test]
    fn a_completed_segment_is_never_split() {
        let mut done = SegmentState::new(0, 0, 8191);
        done.mark_durable_through(8191);
        assert!(done.is_complete());
        // Even claiming it stalled must not make it a candidate: §6.2's
        // prohibition is absolute.
        assert!(plan(&[done], &[(0, 8192)], &[0], true, 8).is_none());
    }

    #[test]
    fn a_segment_with_nothing_durable_is_not_split() {
        // There is no frontier to hand to the first half, so splitting would
        // have to invent one — and inventing one means claiming a byte twice.
        let segment = SegmentState::new(0, 0, 8191);
        assert!(plan(&[segment], &[(0, 0)], &[0], true, 8).is_none());
    }

    #[test]
    fn a_half_below_the_floor_blocks_the_split() {
        // Frontier 512 bytes in: the front half would be under the 1024 floor.
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(511);
        assert!(plan(&[segment], &[(0, 512)], &[0], true, 8).is_none());

        // Frontier 70 bytes in: both halves clear the 1024 floor, so this one
        // *is* splittable — the boundary case on the other side.
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(7167);
        assert!(plan(&[segment], &[(0, 7168)], &[0], true, 8).is_some());

        // Frontier 70 bytes from the end: the back half would be under the
        // floor, so the split is refused even though the segment is clearly
        // lagging.
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(8191 - 70);
        assert!(plan(&[segment], &[(0, 8122)], &[0], true, 8).is_none());
    }

    #[test]
    fn the_segment_cap_stops_splitting() {
        // Three segments, the last of which still has plenty of work left.
        let mut lagging = SegmentState::new(2, 4096, 6143);
        lagging.mark_durable_through(5119);
        let three = vec![
            SegmentState::new(0, 0, 2047),
            SegmentState::new(1, 2048, 4095),
            lagging,
        ];
        let mut full = three.clone();
        full.push(SegmentState::new(3, 6144, 8191));

        // At the cap there is nothing to do, and it is not an error: the task is
        // already running at the parallelism it was configured for.
        assert!(plan(&full, &[(2, 1024), (3, 0)], &[2], true, 4).is_none());
        // One below the cap, the same state splits.
        assert!(plan(&three, &[(2, 1024)], &[2], true, 4).is_some());
        // A configured maximum above the hard ceiling is clamped, never obeyed.
        assert!(
            plan(&three, &[(2, 1024)], &[2], true, HARD_MAX_SEGMENTS + 100).is_some()
        );
    }

    #[test]
    fn the_cooldown_suppresses_the_next_split() {
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(4095);
        let density = Fixed(vec![(0, 4096)]);
        let result = plan_split(
            &[segment.clone()],
            &density,
            &|_| true,
            &|_| true,
            &policy(),
            FLOOR,
            8,
            true,
        );
        assert!(result.is_none(), "a split inside the cooldown must wait");
    }

    #[test]
    fn a_disabled_policy_never_splits() {
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(4095);
        let density = Fixed(vec![(0, 4096)]);
        let disabled = SplitPolicy {
            enabled: false,
            ..policy()
        };
        assert!(
            plan_split(
                &[segment],
                &density,
                &|_| true,
                &|_| true,
                &disabled,
                FLOOR,
                8,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_busy_segment_is_left_alone_unless_it_is_far_behind() {
        let mut segment = SegmentState::new(0, 0, 8191);
        segment.mark_durable_through(4095);
        // Not stalled, and not behind: the lane is doing its job, so another
        // connection would not help.
        assert!(plan(&[segment.clone()], &[(0, 4096)], &[], false, 8).is_none());
        // The same segment, judged behind by the caller's pace test, is worth
        // halving.
        let result = plan(&[segment], &[(0, 4096)], &[], true, 8).unwrap();
        assert_eq!(result.reason, SplitReason::Behind);
    }

    #[test]
    fn the_laggiest_segment_wins() {
        // Two laggards with different amounts left: the bigger one is split,
        // because halving it returns more parallelism.
        let mut small = SegmentState::new(0, 0, 2047);
        small.mark_durable_through(1023);
        let mut large = SegmentState::new(1, 2048, 8191);
        large.mark_durable_through(3071);
        let result = plan(
            &[small, large],
            &[(0, 1024), (1, 1024)],
            &[0, 1],
            false,
            8,
        )
        .unwrap();
        assert_eq!(result.id, 1);
        assert_eq!(result.front_id, 2, "the new id is one past the highest");
    }

    #[test]
    fn new_ids_never_collide() {
        let segments = vec![
            SegmentState::new(0, 0, 9),
            SegmentState::new(7, 10, 19),
            SegmentState::new(3, 20, 29),
        ];
        assert_eq!(next_free_id(&segments), 8);
        assert_eq!(next_free_id(&[]), 0);
    }

    #[test]
    fn the_stall_tracker_measures_a_duration_without_progress() {
        let start = tokio::time::Instant::now();
        let stall = Duration::from_millis(500);
        let mut tracker = LagTracker::new();
        tracker.register(0, 0, start);
        tracker.register(1, 0, start);
        assert!(!tracker.has_stalled(0, start, stall));

        // Segment 0 moves; segment 1 does not.
        let later = start + Duration::from_millis(400);
        let received = |id: u32| if id == 0 { Some(128) } else { Some(0) };
        tracker.observe(&received, later);
        assert!(!tracker.has_stalled(0, later, stall));
        let much_later = start + Duration::from_millis(600);
        tracker.observe(&received, much_later);
        assert!(!tracker.has_stalled(0, much_later, stall));
        assert!(tracker.has_stalled(1, much_later, stall));
        // An unregistered id is not stalled: no data is not the same as bad
        // data.
        assert!(!tracker.has_stalled(42, much_later, stall));
    }

    #[test]
    fn policy_validation_rejects_degenerate_values() {
        assert!(SplitPolicy::default().validate().is_ok());
        for broken in [
            SplitPolicy {
                check_interval: Duration::ZERO,
                ..SplitPolicy::default()
            },
            SplitPolicy {
                lag_ratio: 0.0,
                ..SplitPolicy::default()
            },
            SplitPolicy {
                lag_ratio: 1.5,
                ..SplitPolicy::default()
            },
            SplitPolicy {
                lag_share: 1.0,
                ..SplitPolicy::default()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?}");
        }
    }

    #[test]
    fn reasons_have_stable_tags() {
        assert_eq!(SplitReason::Stalled.as_str(), "stalled");
        assert_eq!(SplitReason::Behind.as_str(), "behind");
    }
}
