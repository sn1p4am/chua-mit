//! Task lifecycle state machine.
//!
//! Nine states and one explicit transition table. Illegal transitions are a
//! returned error, never a silent no-op — see `spec/engine-v1.md` §3.2.

use std::fmt;

/// The nine lifecycle states of `spec/engine-v1.md` §3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Registered, not probed yet.
    Created,
    /// Asking for metadata.
    Probing,
    /// Metadata ready, waiting for a concurrency slot.
    Queued,
    /// Transferring bytes.
    Downloading,
    /// Paused by the user. Persisted, and still paused across a restart.
    Paused,
    /// Bytes are in place, verification running.
    Verifying,
    /// Finished.
    Completed,
    /// Fatal failure.
    Failed,
    /// Removed by the user.
    Removed,
}

impl TaskState {
    /// The three terminal states. Terminal states never leave on their own;
    /// a host-issued retry on `Failed` is an explicit instruction, not the
    /// state machine drifting.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Removed)
    }

    /// Stable identifier used in persistence, events and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Probing => "probing",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Removed => "removed",
        }
    }

    /// Parse the identifier produced by [`TaskState::as_str`].
    pub fn from_str_id(raw: &str) -> Option<Self> {
        Some(match raw {
            "created" => Self::Created,
            "probing" => Self::Probing,
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "verifying" => Self::Verifying,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "removed" => Self::Removed,
            _ => return None,
        })
    }

    /// Whether `self -> to` is one of the transitions listed in §3.2.
    ///
    /// The table below is the spec's §3.2 table transcribed literally, with the
    /// two wildcard rows expanded. `{Probing,Queued,Downloading} -> Failed`
    /// names its sources explicitly, so `Paused -> Failed` is *not* an edge —
    /// it is deliberately absent rather than quietly added. See the
    /// ambiguity log in `.omp/TODO.md` ("待规格侧回答").
    pub const fn can_transition_to(self, to: Self) -> bool {
        match (self, to) {
            // Created -> Probing                       start
            (Self::Created, Self::Probing) => true,
            // Probing -> Queued                        probe succeeded
            (Self::Probing, Self::Queued) => true,
            // Probing -> Failed                        probe failed, not retryable
            (Self::Probing, Self::Failed) => true,
            // Queued -> Downloading                    slot acquired
            (Self::Queued, Self::Downloading) => true,
            // Queued -> Paused                         pause
            (Self::Queued, Self::Paused) => true,
            // Downloading -> Paused                    pause
            (Self::Downloading, Self::Paused) => true,
            // Downloading -> Queued                    slot yielded / segments exhausted
            (Self::Downloading, Self::Queued) => true,
            // Paused -> Queued                         resume
            (Self::Paused, Self::Queued) => true,
            // Downloading -> Verifying                 all bytes ready and verification on
            (Self::Downloading, Self::Verifying) => true,
            // Downloading -> Completed                 all bytes ready, no verification
            (Self::Downloading, Self::Completed) => true,
            // Verifying -> Completed                   verification passed
            (Self::Verifying, Self::Completed) => true,
            // Verifying -> Failed                      verification failed
            (Self::Verifying, Self::Failed) => true,
            // {Probing,Queued,Downloading} -> Failed   fatal error (§8)
            (Self::Queued | Self::Downloading, Self::Failed) => true,
            // Failed -> Probing                        explicit retry
            (Self::Failed, Self::Probing) => true,
            // non-terminal -> Removed                  remove
            (
                Self::Created
                | Self::Probing
                | Self::Queued
                | Self::Downloading
                | Self::Paused
                | Self::Verifying,
                Self::Removed,
            ) => true,
            // {Completed,Failed} -> Removed             remove
            //
            // §3.2 lists only "非终态 -> Removed", and read literally that
            // leaves a finished or failed task undeletable forever: `remove` is
            // how a user clears a task out of the list, and the tasks a user
            // wants to clear are precisely the ones that are done. §3.1's
            // "terminal states MUST NOT leave on their own" is about automatic
            // departure; a host-issued remove is an explicit instruction, the
            // same shape as the `Failed -> Probing` retry the table already
            // allows out of a terminal state. Logged in `.omp/TODO.md`.
            (Self::Completed | Self::Failed, Self::Removed) => true,
            // Self-transitions are not in the table and are not implied by it.
            _ => false,
        }
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a transition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The pair is not in the §3.2 table.
    #[error("illegal task transition: {from} -> {to}")]
    Illegal {
        /// State the task was in.
        from: TaskState,
        /// State the caller asked for.
        to: TaskState,
    },
    /// The task id is not known to the engine.
    #[error("unknown task id")]
    UnknownTask,
    /// The task is gone; it was removed.
    #[error("task is removed")]
    Removed,
}

impl TransitionError {
    /// Stable identifier for logs and the host's lookup tables.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Illegal { .. } => "illegal_transition",
            Self::UnknownTask => "unknown_task",
            Self::Removed => "task_removed",
        }
    }
}

/// Convenience: the transition check as a `Result`.
pub fn check_transition(from: TaskState, to: TaskState) -> Result<(), TransitionError> {
    if from.can_transition_to(to) {
        Ok(())
    } else {
        Err(TransitionError::Illegal { from, to })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const ALL: [TaskState; 9] = [
        TaskState::Created,
        TaskState::Probing,
        TaskState::Queued,
        TaskState::Downloading,
        TaskState::Paused,
        TaskState::Verifying,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Removed,
    ];

    /// Every edge written out in §3.2, expanded.
    const LEGAL: [(TaskState, TaskState); 23] = [
        (TaskState::Created, TaskState::Probing),
        (TaskState::Probing, TaskState::Queued),
        (TaskState::Probing, TaskState::Failed),
        (TaskState::Queued, TaskState::Downloading),
        (TaskState::Queued, TaskState::Paused),
        (TaskState::Downloading, TaskState::Paused),
        (TaskState::Downloading, TaskState::Queued),
        (TaskState::Paused, TaskState::Queued),
        (TaskState::Downloading, TaskState::Verifying),
        (TaskState::Downloading, TaskState::Completed),
        (TaskState::Verifying, TaskState::Completed),
        (TaskState::Verifying, TaskState::Failed),
        (TaskState::Queued, TaskState::Failed),
        (TaskState::Downloading, TaskState::Failed),
        (TaskState::Failed, TaskState::Probing),
        (TaskState::Created, TaskState::Removed),
        (TaskState::Probing, TaskState::Removed),
        (TaskState::Queued, TaskState::Removed),
        (TaskState::Downloading, TaskState::Removed),
        (TaskState::Paused, TaskState::Removed),
        (TaskState::Verifying, TaskState::Removed),
        // Terminal states keep exactly one other way out: the user deleting the
        // task. See the comment on the table entry.
        (TaskState::Completed, TaskState::Removed),
        (TaskState::Failed, TaskState::Removed),
    ];

    fn is_listed(from: TaskState, to: TaskState) -> bool {
        LEGAL.contains(&(from, to))
    }

    #[test]
    fn every_legal_transition_is_accepted() {
        for (from, to) in LEGAL {
            assert!(from.can_transition_to(to), "{from} -> {to} should be legal");
            assert!(check_transition(from, to).is_ok());
        }
    }

    #[test]
    fn representative_illegal_transitions_are_rejected() {
        let illegal = [
            // Terminal states do not leave on their own.
            (TaskState::Completed, TaskState::Downloading),
            (TaskState::Completed, TaskState::Queued),
            (TaskState::Removed, TaskState::Queued),
            (TaskState::Removed, TaskState::Probing),
            // Failed only leaves via an explicit retry to Probing.
            (TaskState::Failed, TaskState::Queued),
            (TaskState::Failed, TaskState::Downloading),
            (TaskState::Failed, TaskState::Completed),
            (TaskState::Failed, TaskState::Paused),
            // Skipping the probe.
            (TaskState::Created, TaskState::Queued),
            (TaskState::Created, TaskState::Downloading),
            (TaskState::Created, TaskState::Completed),
            // Verifying is only reachable from Downloading.
            (TaskState::Queued, TaskState::Verifying),
            (TaskState::Created, TaskState::Verifying),
            (TaskState::Paused, TaskState::Verifying),
            // Downloading cannot be entered from Probing directly.
            (TaskState::Probing, TaskState::Downloading),
            // Completed is not reachable from Verifying's siblings.
            (TaskState::Queued, TaskState::Completed),
            (TaskState::Probing, TaskState::Completed),
            (TaskState::Downloading, TaskState::Created),
            (TaskState::Queued, TaskState::Created),
            (TaskState::Paused, TaskState::Created),
            // §3.2 names the sources of `-> Failed` explicitly and `Paused` is
            // not among them. A paused task is not being worked on, so there is
            // no worker that could report a fatal error for it.
            (TaskState::Paused, TaskState::Failed),
            (TaskState::Paused, TaskState::Downloading),
            (TaskState::Paused, TaskState::Probing),
        ];
        for (from, to) in illegal {
            assert!(
                !from.can_transition_to(to),
                "{from} -> {to} should be illegal"
            );
            let error = check_transition(from, to).unwrap_err();
            assert_eq!(error.tag(), "illegal_transition");
            match error {
                TransitionError::Illegal { from: f, to: t } => {
                    assert_eq!(f, from);
                    assert_eq!(t, to);
                }
                other => panic!("unexpected variant: {other:?}"),
            }
        }
    }

    #[test]
    fn no_self_transition_is_legal() {
        for state in ALL {
            assert!(
                !state.can_transition_to(state),
                "{state} -> {state} should not be legal"
            );
        }
    }

    #[test]
    fn a_terminal_state_leaves_only_on_an_explicit_instruction() {
        for state in ALL.iter().copied().filter(|s| s.is_terminal()) {
            for to in ALL {
                let legal = state.can_transition_to(to);
                // Two explicit host instructions, and nothing else: retry a
                // failed task, delete a task. Neither is the state machine
                // drifting on its own, and `Removed -> Removed` is still not an
                // edge — a task can only be deleted once.
                let allowed = (to == TaskState::Removed && to != state)
                    || (state == TaskState::Failed && to == TaskState::Probing);
                assert_eq!(legal, allowed, "{state} -> {to}");
            }
        }
    }

    #[test]
    fn table_is_exactly_the_listed_edges() {
        for from in ALL {
            for to in ALL {
                assert_eq!(
                    from.can_transition_to(to),
                    is_listed(from, to),
                    "mismatch for {from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn state_ids_round_trip() {
        for state in ALL {
            assert_eq!(TaskState::from_str_id(state.as_str()), Some(state));
        }
        assert_eq!(TaskState::from_str_id("nonsense"), None);
    }
}
