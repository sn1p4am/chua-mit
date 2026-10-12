//! `chua-cli` — the M1 verification host for the download engine.
//!
//! This is not a product interface. It exists so the engine can be driven as a
//! black box: `spec/acceptance/run.py` runs this binary and judges the engine by
//! what ends up on disk. Its argument shape and its exit codes are a contract
//! (`spec/cli-v1.md` §6) and everything else here is deliberately plain.
//!
//! The wiring is short on purpose: parse, install a log subscriber, build a
//! runtime, run one command, and exit with the code that command earned.

mod cli;
mod error;
mod host;
mod render;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let args = match cli::Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            // clap exits 2 for a parse error and 0 for `--help`, and neither is
            // what `spec/cli-v1.md` §3 says: a usage error is 1, and help is not
            // an error at all.
            let code: u8 = if error.use_stderr() { 1 } else { 0 };
            if error.print().is_err() {
                return ExitCode::from(2);
            }
            return ExitCode::from(code);
        }
    };

    host::init_logging();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("chua: cannot start the async runtime: {error}");
            return error::Code::TaskFailed.exit();
        }
    };

    match runtime.block_on(host::run(args.command)) {
        Ok(code) => code.exit(),
        Err(failure) => {
            eprintln!("chua: {}", failure.message);
            failure.code.exit()
        }
    }
}
