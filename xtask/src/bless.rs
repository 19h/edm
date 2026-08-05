//! Regenerating the oracle fixtures, and checking the one contract that cannot
//! be a fixture.
//!
//! The fixtures are measurements of a specific JavaScript engine. Regenerating
//! them under a different engine and committing the result would quietly change
//! what the test suite means, so the recorded version is checked first.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const GENERATORS: [&str; 3] = ["bless-js.ts", "bless-id64.ts", "bless-ardent.ts"];

pub(crate) fn run(force: bool) -> Result<()> {
    let root = crate::repo_root()?;
    let fixtures = root.join("crates").join("edm-core").join("tests").join("fixtures");
    let installed = bun_version(&root)?;

    match recorded_version(&fixtures)? {
        Some(recorded) if recorded != installed => {
            if !force {
                bail!(
                    "the committed fixtures were measured under bun {recorded} and this is \
                     bun {installed}.\nRegenerating would silently restate what every oracle \
                     test means. Re-run with --force if that is the intention, and say so in \
                     the commit message."
                );
            }
            println!("  bun {recorded} -> {installed} (forced)");
        }
        Some(recorded) => println!("  bun {recorded}, unchanged"),
        None => println!("  no recorded bun version; treating as first generation"),
    }

    for generator in GENERATORS {
        let script = root.join("xtask").join("oracle").join(generator);
        println!("  {generator}");
        let status = Command::new("bun")
            .arg(&script)
            .arg(&fixtures)
            .current_dir(&root)
            .status()
            .with_context(|| format!("running {}", script.display()))?;
        if !status.success() {
            bail!("{generator} failed");
        }
    }
    Ok(())
}

/// **[C1]** — the Ardent module is imported at runtime by the TypeScript and
/// compiled into the Rust, and this is what keeps the two the same thing.
///
/// Regenerates the contract fixture into a scratch directory and compares it to
/// the committed one. A difference means `ardent.ts` changed under the port's
/// feet: the divergence that C1 registers is "compiled in rather than
/// imported", not "allowed to drift".
pub(crate) fn ardent_contract() -> Result<()> {
    let root = crate::repo_root()?;
    let committed =
        root.join("crates").join("edm-core").join("tests").join("fixtures").join("ardent_contract.tsv");
    let scratch = root.join("target").join("xtask-ardent");
    std::fs::create_dir_all(&scratch)?;

    let status = Command::new("bun")
        .arg(root.join("xtask").join("oracle").join("bless-ardent.ts"))
        .arg(&scratch)
        .current_dir(&root)
        .status()
        .context("running bless-ardent.ts")?;
    if !status.success() {
        bail!("bless-ardent.ts failed");
    }

    let fresh = std::fs::read_to_string(scratch.join("ardent_contract.tsv"))?;
    let old = std::fs::read_to_string(&committed)
        .with_context(|| format!("reading {}", committed.display()))?;
    if fresh == old {
        println!("  pass  ardent.ts matches the compiled-in port");
        return Ok(());
    }

    let first = fresh
        .lines()
        .zip(old.lines())
        .find(|(new, old)| new != old)
        .map_or_else(|| "(length differs)".to_owned(), |(new, old)| format!("{old}\n  now: {new}"));
    bail!("ardent.ts has changed since the contract was recorded:\n  was: {first}");
}

fn bun_version(root: &Path) -> Result<String> {
    let output = Command::new("bun")
        .arg("--version")
        .current_dir(root)
        .output()
        .context("running bun --version")?;
    if !output.status.success() {
        bail!("bun --version failed");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_matches(|c: char| c.is_ascii_whitespace()).to_owned())
}

/// The version the committed fixtures record, if they agree on one.
fn recorded_version(fixtures: &Path) -> Result<Option<String>> {
    let mut found: Option<String> = None;
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures)
        .with_context(|| format!("reading {}", fixtures.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    files.sort();

    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        // The banner is in the first few lines of every generated fixture.
        for line in text.lines().take(4) {
            let Some(version) = version_in(line) else { continue };
            match &found {
                Some(existing) if *existing != version => bail!(
                    "the fixtures disagree about which bun produced them ({existing} and \
                     {version}); regenerate all of them before trusting any"
                ),
                Some(_) => {}
                None => found = Some(version),
            }
        }
    }
    Ok(found)
}

/// Pulls `1.2.3` out of `# … under bun 1.2.3` or `# bun 1.2.3`.
fn version_in(line: &str) -> Option<String> {
    let rest = line.split_once("bun ")?.1;
    let version: String =
        rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    (version.split('.').count() == 3 && !version.ends_with('.')).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_is_read_in_both_spellings() {
        assert_eq!(
            version_in("# generated by xtask/oracle/bless-js.ts under bun 1.2.3").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(version_in("# bun 1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(version_in("# sliced verbatim from x under bun 1.20.30").as_deref(), Some("1.20.30"));
        assert_eq!(version_in("# nothing here"), None);
        assert_eq!(version_in("# bun oven"), None);
    }

    #[test]
    fn the_committed_fixtures_agree_on_one_engine() {
        let fixtures =
            crate::repo_root().unwrap().join("crates/edm-core/tests/fixtures");
        assert!(recorded_version(&fixtures).unwrap().is_some());
    }
}
