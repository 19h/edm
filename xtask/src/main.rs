//! `cargo xtask <subcommand>` — the development tasks that need more than
//! `cargo test` can express.
//!
//! * `parity` runs the same argv through `game-internal-api.ts` under Bun and
//!   through the Rust binary, against one mock server, and diffs stdout,
//!   stderr, the exit code, any `--dump` file and the wire log. It is the
//!   acceptance gate for the whole port.
//! * `bless` regenerates the oracle fixtures, and with `--golden` the
//!   committed output of the one command that has no oracle.
//! * `gates` runs the structural checks.
//! * `mock` serves the scenarios by hand, for poking at with `curl`.
//! * `ardent-contract` re-executes the real `ardent.ts` and checks the
//!   compiled-in port still agrees with it. **[C1]**

mod bless;
mod codec;
mod gates;
mod mock;
mod parity;
mod scenario;
mod toml;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

const USAGE: &str = "\
cargo xtask <command>

  parity [--suite all|cli] [--filter <substring>] [--list]
        Run every scenario through both implementations and diff the results.
        `--suite cli` is the subset that makes no requests, and needs no mock.

  bless [--golden] [--force]
        Regenerate the oracle fixtures from the JavaScript engine. Refuses when
        the installed bun differs from the one the fixtures record.
        `--golden` instead regenerates the committed output of the scenarios
        that have no oracle (C25), from the Rust side alone.

  gates
        Purity of edm-core, tokio's `signal` feature (R96), and a scan for
        committed credentials.

  mock [--scenario <name>]
        Serve a scenario's script on an ephemeral port until interrupted.

  ardent-contract
        Re-run the real ardent.ts and check the compiled-in port matches (C1).
";

fn main() -> ExitCode {
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<()> {
    // `args_os` and a lossy decode, for the same reason the program itself uses
    // them: `std::env::args()` panics on a non-UTF-8 argument. **[R55]**
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut rest = argv.iter().skip(1);

    match argv.first().map(String::as_str) {
        Some("parity") => {
            let mut options = parity::Options {
                suite: parity::Suite::All,
                filter: None,
                list: false,
            };
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--suite" => {
                        options.suite = match rest.next().map(String::as_str) {
                            Some("all") => parity::Suite::All,
                            Some("cli") => parity::Suite::Cli,
                            other => bail!("--suite takes `all` or `cli`, got {other:?}"),
                        };
                    }
                    "--filter" => {
                        options.filter =
                            Some(rest.next().context("--filter needs a substring")?.clone());
                    }
                    "--list" => options.list = true,
                    other => bail!("unknown option {other}\n\n{USAGE}"),
                }
            }
            parity::run(&options)
        }
        Some("bless") => {
            let mut force = false;
            let mut golden = false;
            for flag in rest {
                match flag.as_str() {
                    "--force" => force = true,
                    "--golden" => golden = true,
                    other => bail!("unknown option {other}\n\n{USAGE}"),
                }
            }
            if golden {
                bless::goldens(force)
            } else {
                bless::run(force)
            }
        }
        Some("gates") => gates::run(),
        Some("mock") => {
            let mut name = None;
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--scenario" => {
                        name = Some(rest.next().context("--scenario needs a name")?.clone());
                    }
                    other => bail!("unknown option {other}\n\n{USAGE}"),
                }
            }
            serve(name.as_deref())
        }
        Some("ardent-contract") => bless::ardent_contract(),
        Some("help" | "--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

/// Serves one scenario's script until interrupted, for hand-driving a client.
fn serve(name: Option<&str>) -> Result<()> {
    let root = repo_root()?;
    let scenarios = scenario::load_all(&root.join("xtask").join("scenarios"))?;
    let mock = mock::Mock::start()?;

    if let Some(name) = name {
        let scenario = scenarios
            .iter()
            .find(|scenario| scenario.name == name)
            .with_context(|| format!("no scenario called {name}"))?;
        mock.load(scenario);
        println!("serving `{}` on {}", scenario.name, mock.base_url());
    } else {
        println!("serving an empty script on {}", mock.base_url());
    }
    println!(
        "  EDM_ORIGIN_OVERRIDE={0}  EDM_ARDENT_BASE={0}/v2  EDM_EDDN_URL={0}/upload/  EDM_SPANSH_BASE={0}/api",
        mock.base_url()
    );
    println!("interrupt to stop");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The workspace root, from this crate's manifest directory.
pub(crate) fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_owned)
        .context("xtask has no parent directory")
}
