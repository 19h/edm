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
