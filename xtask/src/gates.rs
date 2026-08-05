//! Structural checks that no unit test can express.
//!
//! Each one guards a property of the *build* rather than of a function: that
//! the pure crate stayed pure, that a feature nobody wants cannot be switched
//! on by accident, and that no credential was ever committed.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub(crate) fn run() -> Result<()> {
    let root = crate::repo_root()?;
    let mut failures = Vec::new();

    for (name, check) in [
        ("purity", purity as fn(&Path) -> Result<()>),
        ("no-signal-handling", no_signal_handling),
        ("no-committed-credentials", no_committed_credentials),
        ("parity-isolation", parity_isolation),
        ("no-dotenv-leakage", no_dotenv_leakage),
    ] {
        match check(&root) {
            Ok(()) => println!("  pass  {name}"),
            Err(error) => {
                println!("  FAIL  {name}");
                failures.push(format!("{name}: {error:#}"));
            }
        }
    }

    if failures.is_empty() {
        return Ok(());
    }
    println!();
    for failure in &failures {
        println!("{failure}");
    }
    bail!("{} gates failed", failures.len())
}

/// Every `bun` the harness spawns must be told to ignore `.env`.
///
/// Bun loads `.env` from its working directory before the script runs, and both
/// the parity harness and the fixture blessers run in the repository root. A
/// developer's `.env` there — this project's own step-0 check needs one — then
/// reaches the Bun side of a differential comparison and not the Rust side, so
/// the harness reports a divergence it created. Worse, the values in that file
/// are live credentials.
///
/// `--env-file` replaces the default set outright, so naming an empty file
/// loads nothing. This gate is a grep because the property is syntactic: a
/// `bun` spawned without it is the bug, and no run-time assertion can see the
/// spawn that was never guarded.
fn no_dotenv_leakage(root: &Path) -> Result<()> {
    let dir = root.join("xtask").join("src");
    let mut unguarded = Vec::new();

    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        for (number, line) in text.lines().enumerate() {
            // `bun --version` reads no script and so loads no environment.
            if line.contains("Command::new(\"bun\")") || line.contains("Command::new(bun)") {
                let window: String = text
                    .lines()
                    .skip(number)
                    .take(12)
                    .collect::<Vec<_>>()
                    .join("\n");
                if !window.contains("--env-file")
                    && !window.contains("no_dotenv")
                    && !window.contains("--version")
                {
                    unguarded.push(format!("{}:{}", path.display(), number + 1));
                }
            }
        }
    }

    if unguarded.is_empty() {
        return Ok(());
    }
    bail!(
        "these spawn bun without `--env-file`, so a `.env` in the repository root \
         would reach it:\n  {}",
        unguarded.join("\n  ")
    )
}

/// `edm-core` computes; it does not do.
///
/// The whole port is arranged so that everything observable is a pure function
/// of inputs, which is what makes the oracle fixtures meaningful. A single
/// `getrandom` call inside `edm-core` would silently reintroduce something the
/// fixtures cannot pin. Dev edges are excluded because `jsonschema` — used only
/// to validate the EDDN message against its published schema — drags in
/// reqwest, and a test-only dependency cannot make the shipped crate impure.
fn purity(root: &Path) -> Result<()> {
    let tree = cargo_tree(root, &["-p", "edm-core", "-e", "normal"])?;
    let forbidden: Vec<&str> = ["tokio", "reqwest", "rustix", "getrandom"]
        .into_iter()
        .filter(|name| {
            tree.lines().any(|line| line.split_whitespace().any(|word| word == *name))
        })
        .collect();
    if forbidden.is_empty() {
        return Ok(());
    }
    bail!("edm-core's normal dependency tree reaches {}", forbidden.join(", "))
}

/// **[R96]** — no signal handling at all.
///
/// The original installs no handler, so a `SIGINT` mid-sweep kills the process
/// and loses the run. Reproducing that is not a decision anyone has to remember:
/// with tokio's `signal` feature off, a graceful-shutdown handler is a
/// compile error rather than a review comment. This gate is what keeps the
/// feature off when a future dependency starts asking for it.
fn no_signal_handling(root: &Path) -> Result<()> {
    let tree = cargo_tree(root, &["-p", "edm", "-e", "features"])?;
    if tree.contains(r#"tokio feature "signal""#) {
        bail!("something in edm's tree enables tokio's `signal` feature");
    }
    Ok(())
}

/// No credential-shaped literal anywhere in the tracked tree.
///
/// The Companion API's machine and auth tokens are validated at exactly 80 and
/// 2024 characters (ts:86, ts:90), so a leaked one is an unbroken printable run
/// of exactly that length. Scanning for the *shape* rather than for known
/// values is the point: a token nobody has seen yet is still caught.
fn no_committed_credentials(root: &Path) -> Result<()> {
    let listing = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .context("running git ls-files")?;
    if !listing.status.success() {
        bail!("git ls-files failed");
    }

    let mut hits = Vec::new();
    for name in listing.stdout.split(|byte| *byte == 0) {
        if name.is_empty() {
            continue;
        }
        let relative = String::from_utf8_lossy(name).into_owned();
        let Ok(bytes) = std::fs::read(root.join(&relative)) else { continue };
        for length in credential_runs(&bytes) {
            hits.push(format!("{relative}: a {length}-character printable run"));
        }
    }
    if hits.is_empty() {
        return Ok(());
    }
    bail!("{}", hits.join("\n"));
}

/// The lengths of every maximal printable run that looks like a credential.
///
/// Length alone is not enough, and the first run of this gate proved it: an
/// eighty-column table rule is eighty printable characters, and so is a line of
/// the JSON corpus. Two more conditions turn the false positives off without
/// weakening the check, because both are properties a token has and neither
/// text nor a rule does — a restricted alphabet (the Companion API's tokens are
/// base64-shaped, so `{`, `"`, `:` and `|` rule a run out) and enough distinct
/// characters that a repeated separator cannot qualify.
fn credential_runs(bytes: &[u8]) -> Vec<usize> {
    const MIN_DISTINCT: usize = 16;
    let token_byte = |byte: u8| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'.' | b'-' | b'~')
    };

    let mut found = Vec::new();
    let mut run: Vec<u8> = Vec::new();
    for byte in bytes.iter().copied().chain(std::iter::once(b'\n')) {
        if byte.is_ascii_graphic() {
            run.push(byte);
            continue;
        }
        if (run.len() == 80 || run.len() == 2024) && run.iter().copied().all(token_byte) {
            let mut distinct: Vec<u8> = run.clone();
            distinct.sort_unstable();
            distinct.dedup();
            if distinct.len() >= MIN_DISTINCT {
                found.push(run.len());
            }
        }
        run.clear();
    }
    found
}

fn cargo_tree(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .arg("tree")
        .args(args)
        .current_dir(root)
        .output()
        .context("running cargo tree")?;
    if !output.status.success() {
        bail!("cargo tree {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A new command must not widen the grammar the old ones are held to.
///
/// `cargo xtask parity` diffs the same argv through Bun and through this
/// binary. `route` has no oracle — Bun answers `Unknown command "route"` — so
/// it is dispatched from a table disjoint from `KNOWN_COMMANDS` and its flag
/// names resolve only when the command is `route` \[C25, C26\].
///
/// The risk this gate exists for is specific and quiet: widening
/// `Flag::resolve` globally would make `edm market Colonia --pad L` succeed
/// where the TypeScript exits 2 — a fidelity regression on argv the harness
/// never runs, and therefore one no scenario would catch.
fn parity_isolation(root: &Path) -> Result<()> {
    use edm_core::cli::{self, Flag, Table};

    // 1. The pinned command set is exactly the four the TypeScript dispatches.
    if cli::KNOWN_COMMANDS != ["market", "list", "markets", "trade"] {
        bail!("KNOWN_COMMANDS has changed; R48's ordering and the parity scenarios depend on it");
    }
    for command in cli::EXTENDED_COMMANDS {
        if cli::is_known_command(command) {
            bail!("{command} appears in both KNOWN_COMMANDS and EXTENDED_COMMANDS");
        }
    }

    // 2. No route-only name may shadow a base name. A shadow would change what
    //    an existing command does, silently.
    for flag in Flag::ALL {
        let display = flag.display().trim_start_matches('-');
        let normalized = cli::normalize(display);
        if let (Some(base), Some(route)) =
            (Flag::resolve(&normalized), Flag::resolve_route(&normalized))
            && base != route
        {
            bail!("{display} resolves to {base:?} in the base table and {route:?} in the route table");
        }
    }

    // 3. The strongest statement available: over every argv the harness
    //    actually runs, the two tables agree — *except* on a `route` argv,
    //    which is the one case the extended table exists for. A route argv
    //    instead has to satisfy the complementary claim, that the extended
    //    parse it reached is only ever reached for `route`.
    let scenarios = crate::scenario::load_all(&root.join("xtask").join("scenarios"))?;
    for scenario in &scenarios {
        let dispatched = cli::parse_dispatch(&scenario.argv);

        if let Some(route) = &dispatched.route {
            // The dispatcher hands the extended parse to the command layer, so
            // this is the whole of what "route-only" means at run time.
            if route.command != "route" {
                bail!(
                    "scenario {} took the extended arm as command {:?}, not route: {:?}",
                    scenario.name,
                    route.command,
                    scenario.argv
                );
            }
            continue;
        }

        let base = cli::parse(&scenario.argv);
        let extended = cli::parse_with(&scenario.argv, Table::Extended);
        let same = match (&base, &extended) {
            (Ok(a), Ok(b)) => a.command == b.command && a.positionals == b.positionals,
            (Err(a), Err(b)) => a.to_string() == b.to_string(),
            _ => false,
        };
        if !same {
            bail!(
                "scenario {} parses differently under the extended table: {:?}",
                scenario.name,
                scenario.argv
            );
        }
    }

    // 4. And the case no scenario covers, because it is a *failure* the harness
    //    would have nothing to compare: a route-only flag on a ported command
    //    must still be an unknown option. This is the regression the whole
    //    two-table arrangement exists to prevent, so it is asserted directly
    //    rather than inferred from the scenarios that happen to be committed.
    for argv in [
        vec!["market".to_owned(), "Colonia".to_owned(), "--radius".to_owned(), "30".to_owned()],
        vec!["markets".to_owned(), "--pad".to_owned(), "L".to_owned()],
        vec!["trade".to_owned(), "--min-profit".to_owned(), "1000".to_owned()],
    ] {
        let dispatched = cli::parse_dispatch(&argv);
        if dispatched.route.is_some() || dispatched.base.is_ok() {
            bail!("a route-only flag was accepted on a ported command: {argv:?}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token-shaped run of exactly `length` characters.
    fn token(length: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        (0..length)
            // A fixed irrational-ish stride, so the run is not a repeating
            // pattern and covers the alphabet the way a real token does.
            .map(|index| char::from(ALPHABET[(index * 37) % ALPHABET.len()]))
            .collect()
    }

    #[test]
    fn a_credential_shaped_run_is_found_at_either_width() {
        assert_eq!(credential_runs(format!("{}\n", token(80)).as_bytes()), vec![80]);
        assert_eq!(credential_runs(format!("{}\n", token(2024)).as_bytes()), vec![2024]);
        // A run at end-of-file with no trailing newline still counts.
        assert_eq!(credential_runs(token(80).as_bytes()), vec![80]);
    }

    #[test]
    fn text_that_merely_happens_to_be_eighty_wide_is_not_a_credential() {
        assert_eq!(credential_runs(b"short\n"), Vec::<usize>::new());
        // Only the exact widths the API validates.
        assert_eq!(credential_runs(format!("{}\n", token(81)).as_bytes()), Vec::<usize>::new());
        // An eighty-column table rule.
        assert_eq!(credential_runs(format!("{}\n", "=".repeat(80)).as_bytes()), Vec::<usize>::new());
        // A line of the JSON corpus: right length, wrong alphabet.
        let json = format!("{{\"a\":\"{}\"}}", token(72));
        assert_eq!(json.len(), 80);
        assert_eq!(credential_runs(format!("{json}\n").as_bytes()), Vec::<usize>::new());
    }
}

