//! The command-line surface: `spec/cli-v1.md` §2.
//!
//! Deliberately plain. This binary exists so the engine can be driven as a
//! black box — `spec/acceptance/run.py` is the real consumer — so there are no
//! prompts, no colour, and no configuration wizard. The shape of the arguments
//! and the exit codes are a contract; everything else is decoration.
//!
//! # Output is ASCII
//!
//! Every string this host prints is ASCII, including the help text. The
//! acceptance suite captures the child's stdout and decodes it with the
//! platform's locale codec, which on a Chinese Windows install is GBK: UTF-8
//! prose raises `UnicodeDecodeError` inside the harness and takes the whole run
//! down with it. A host whose diagnostics cannot be read by the tool that
//! judges it is not much of a host, so the wording lives in English here and
//! the Chinese product wording stays where it belongs (`spec/engine-v1.md`
//! §8.4's "host owns the wording" applies to the M5 UI, and this is not it).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::error::CliError;

/// Top-level arguments.
#[derive(Debug, Parser)]
#[command(
    name = "chua",
    version,
    about = "chua download engine: command-line host (spec/cli-v1.md)",
    long_about = "Drives the download engine as a black box. The state directory is \
                  CHUA_DATA_DIR when set, and the user data directory otherwise.",
    disable_help_subcommand = true,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Args {
    /// Which command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands of §2, and nothing else.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a task and download it; blocks until the task is finished
    Add {
        /// Source URL
        url: String,
        /// Target file; omitted means the engine derives a name in the current directory
        dest: Option<PathBuf>,
        /// Register the task and return without waiting for a result
        #[arg(long)]
        detach: bool,
        /// Override this task's segment ceiling (1..=64)
        #[arg(long, value_name = "N")]
        segments: Option<u32>,
        /// This task's rate ceiling, e.g. 2M / 500K / 0 (0 = unlimited)
        #[arg(long, value_name = "RATE")]
        limit: Option<String>,
    },
    /// List every task
    List,
    /// Show one task in detail
    Status {
        /// Task id
        id: u64,
    },
    /// Pause a task
    Pause {
        /// Task id
        id: u64,
    },
    /// Resume; <id> or --all, one of the two, and wait for the task to finish
    Resume {
        /// Task id
        id: Option<u64>,
        /// Resume every non-terminal task in the store
        #[arg(long)]
        all: bool,
    },
    /// Remove a task; a task with unfinished data needs --force
    Remove {
        /// Task id
        id: u64,
        /// Allow removing a task that has unfinished data
        #[arg(long)]
        force: bool,
    },
}

/// Parse a rate written the way `spec/cli-v1.md` §2 writes it: `2M`, `500K`,
/// `0`, or a plain byte count.
///
/// The suffixes are binary, because that is what a transfer ceiling means in
/// practice and what the rest of the engine reports. `0` is not a rate of zero
/// bytes per second — that would stall every transfer forever — it is the
/// documented spelling of "no limit".
pub fn parse_limit(raw: Option<&str>) -> Result<Option<u64>, CliError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage(
            "--limit must not be empty (0 means unlimited)",
        ));
    }
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, suffix) = trimmed.split_at(split);
    if digits.is_empty() {
        return Err(CliError::usage(format!("--limit cannot be parsed: {raw}")));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| CliError::usage(format!("--limit is out of range: {raw}")))?;
    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => {
            return Err(CliError::usage(format!(
                "--limit has an unknown unit: {other} (use K, M or G)"
            )));
        }
    };
    let rate = value
        .checked_mul(multiplier)
        .ok_or_else(|| CliError::usage(format!("--limit is out of range: {raw}")))?;
    Ok((rate > 0).then_some(rate))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse_the_way_the_spec_writes_them() {
        assert_eq!(parse_limit(None).unwrap(), None);
        assert_eq!(parse_limit(Some("2M")).unwrap(), Some(2 * 1024 * 1024));
        assert_eq!(parse_limit(Some("500K")).unwrap(), Some(500 * 1024));
        assert_eq!(parse_limit(Some("1G")).unwrap(), Some(1024 * 1024 * 1024));
        assert_eq!(parse_limit(Some("4096")).unwrap(), Some(4096));
        assert_eq!(parse_limit(Some(" 64 KiB ")).unwrap(), Some(64 * 1024));
        // Zero is the documented spelling of "no limit", not a dead stop.
        assert_eq!(parse_limit(Some("0")).unwrap(), None);
    }

    #[test]
    fn bad_rates_are_usage_errors_not_silent_defaults() {
        for raw in ["", "  ", "M", "2X", "2M2", "-1", "99999999999999999999999"] {
            let error = parse_limit(Some(raw)).unwrap_err();
            assert_eq!(error.code, crate::error::Code::Usage, "{raw}");
        }
    }
}
