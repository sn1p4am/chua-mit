//! Exit codes and the error the handlers pass back to `main`.
//!
//! The codes are the contract `spec/cli-v1.md` §3 fixes, and the acceptance
//! suite reads them. Keeping them in one type rather than sprinkling
//! `ExitCode::from(2)` through the handlers is what stops "usage error" and
//! "task failed" from quietly collapsing into each other, which is the failure
//! mode that section exists to prevent.

use std::process::ExitCode;

/// `spec/cli-v1.md` §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The command did what it was asked to do.
    Success = 0,
    /// Unknown subcommand, or invalid arguments.
    Usage = 1,
    /// The engine reported a structured failure for the task.
    TaskFailed = 2,
    /// The task id is not known.
    NotFound = 3,
    /// The destination file exists and this host does not overwrite.
    Conflict = 4,
}

impl Code {
    /// The process exit code.
    pub fn exit(self) -> ExitCode {
        ExitCode::from(self as u8)
    }
}

/// Something the command could not do, with the code that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// What the process should exit with.
    pub code: Code,
    /// One line for the user. The CLI owns its wording (`spec/cli-v1.md` §1);
    /// the engine only ever hands over structured reasons.
    pub message: String,
}

impl CliError {
    /// A bad invocation: the arguments do not describe a valid request.
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: Code::Usage,
            message: message.into(),
        }
    }

    /// The task could not be carried out.
    pub fn task(message: impl Into<String>) -> Self {
        Self {
            code: Code::TaskFailed,
            message: message.into(),
        }
    }

    /// No such task.
    pub fn not_found(id: chua_engine::TaskId) -> Self {
        Self {
            code: Code::NotFound,
            message: format!("no such task: {id}"),
        }
    }

    /// The destination is occupied. §7.4 leaves the decision to the host and
    /// this host's decision is never to overwrite.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            code: Code::Conflict,
            message: message.into(),
        }
    }

    /// A structured engine failure, rendered by the host.
    pub fn failure(failure: &chua_engine::Failure, context: &str) -> Self {
        Self {
            code: Code::TaskFailed,
            message: format!("{context}: {}", crate::render::failure(failure)),
        }
    }
}
