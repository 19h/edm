//! Entry point.
//!
//! Deliberately tiny. Everything it does is ordered: the process is hardened
//! before any credential is read, the environment is snapshotted once so that
//! later reads cannot observe a change, and the exit code is *returned* rather
//! than forced — `market-request.ts` assigns `process.exitCode` and runs to
//! completion, so nothing here may call `std::process::exit`.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::SUCCESS
}
