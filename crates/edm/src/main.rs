//! Entry point.
//!
//! Deliberately tiny. Everything it does is ordered: the process is hardened
//! before any credential is read, the environment is snapshotted once so that
//! later reads cannot observe a change, and the exit code is *returned* rather
//! than forced — `game-internal-api.ts` assigns `process.exitCode` and runs to
//! completion, so nothing here may call `std::process::exit`.

use std::process::ExitCode;

use edm_core::cli::{self, EnvSnapshot};
use edm_core::render;

use edm::cmd::{self, Overrides};
use edm::net::live::LiveHttp;
use edm::out::Out;
use edm::ports::Ports;
use edm::sys;

/// A single-threaded runtime, and not for want of cores.
///
/// The output interleaving is the acceptance gate: a sweep's progress lines,
/// its requeue lines and any stderr diagnostic have to appear in the order the
/// original produces them, and a work-stealing scheduler would make that order
/// depend on how the OS happened to schedule sixteen futures. Single-threaded
/// also means the sweep's `Cell` counters need no atomics to be sound.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    sys::harden();

    // Both decoded lossily: a JavaScript `process.argv` substitutes U+FFFD
    // where `std::env::args` panics, and `clippy.toml` denies the panicking
    // form \[R55\].
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    // First-wins per name, because a raw environment block can repeat one and
    // `getenv` answers with the first \[R55\].
    let env = EnvSnapshot::from_pairs(std::env::vars_os().map(|(name, value)| {
        (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())
    }));

    let overrides = Overrides::from_env(&env);
    // Sampled once, here, and never again: the TypeScript reads it into a
    // module-level constant and ignores SIGWINCH, so a resize mid-sweep
    // reflows nothing \[R31\].
    let width = render::terminal_width(env.get("COLUMNS"), sys::columns());

    // Parsed before `Out` exists because `Out` needs to know about `--json`
    // and `openSession` — where the TypeScript reads it — cannot run until the
    // credentials have loaded.
    let parsed = cli::parse_dispatch(&argv);
    let out = Out::new(width, overrides.metric, cmd::wants_json(&parsed));

    match LiveHttp::new() {
        Ok(http) => {
            let ports = Ports::real();
            cmd::run(parsed, &env, &http, &ports, &out, &overrides).await;
        }
        Err(error) => {
            // A client that will not build is `fetch` failing before it is ever
            // called; it takes the same path as any other thrown error \[R82\].
            out.error(&error.to_string());
            out.set_exit(edm::out::EXIT_FAILURE);
        }
    }

    out.flush();
    out.exit_code()
}
