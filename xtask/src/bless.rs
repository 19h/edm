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

/// Where the goldens record which engine was asked and found to have no answer.
const GOLDEN_ENGINE: &str = "bun-version.txt";

/// Arguments that stop Bun loading `.env` from the working directory.
///
/// A blessing run works in the repository root, where a developer's `.env` may
/// hold live credentials. Fixtures generated with those in scope would be wrong
/// in a way nothing downstream could detect. The `no-dotenv-leakage` gate keeps
/// every `bun` spawn here honest; see `parity.rs`, which is where it was found.
fn no_dotenv(root: &Path) -> [std::ffi::OsString; 2] {
    [
        std::ffi::OsString::from("--env-file"),
        root.join("xtask")
            .join("oracle")
            .join("empty.env")
            .into_os_string(),
    ]
}

pub(crate) fn run(force: bool) -> Result<()> {
    let root = crate::repo_root()?;
    let fixtures = root
        .join("crates")
        .join("edm-core")
        .join("tests")
        .join("fixtures");
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
            .args(no_dotenv(&root))
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

/// Regenerates the goldens for the scenarios that have no oracle \[C25\].
///
/// The engine check is the same shape as the fixture one above, and for a
/// related reason. A golden exists *because* the installed engine has no answer
/// for this argv — `bun --version` is the record of which engine was asked. If
/// a later engine grew the command, the differential gate would have become
/// applicable again, and silently regenerating goldens under it would bury
/// that: the suite would keep asserting the Rust against itself.
pub(crate) fn goldens(force: bool) -> Result<()> {
    let root = crate::repo_root()?;
    let dir = root.join("xtask").join("scenarios").join("golden");
    let installed = bun_version(&root)?;

    match recorded_engine(&dir) {
        Some(recorded) if recorded != installed => {
            if !force {
                bail!(
                    "the committed goldens were blessed against bun {recorded} and this is \
                     bun {installed}.\nA golden stands in for an oracle that had no answer; \
                     check that bun {installed} still answers `Unknown command \"route\"` \
                     before replacing them. Re-run with --force once it is, and say so in \
                     the commit message."
                );
            }
            println!("  bun {recorded} -> {installed} (forced)");
        }
        Some(recorded) => println!("  bun {recorded}, unchanged"),
        None => println!("  no recorded bun version; treating as first generation"),
    }

    let written = crate::parity::bless_goldens(&root)?;
    for name in &written {
        println!("  {name}");
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join(GOLDEN_ENGINE),
        format!(
            "# The engine `cargo xtask bless --golden` found to have no answer for these\n\
             # scenarios' argv. Rewritten by that command; do not hand-edit.\n\
             bun {installed}\n"
        ),
    )?;
    Ok(())
}

/// The engine the committed goldens were blessed against, if any are committed.
fn recorded_engine(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(GOLDEN_ENGINE)).ok()?;
    text.lines().find_map(version_in)
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
    let committed = root
        .join("crates")
        .join("edm-core")
        .join("tests")
        .join("fixtures")
        .join("ardent_contract.tsv");
    let scratch = root.join("target").join("xtask-ardent");
    std::fs::create_dir_all(&scratch)?;

    let status = Command::new("bun")
        .args(no_dotenv(&root))
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
        .map_or_else(
            || "(length differs)".to_owned(),
            |(new, old)| format!("{old}\n  now: {new}"),
        );
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
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_owned())
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
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        // The banner is in the first few lines of every generated fixture.
        for line in text.lines().take(4) {
            let Some(version) = version_in(line) else {
                continue;
            };
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
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
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
        assert_eq!(
            version_in("# sliced verbatim from x under bun 1.20.30").as_deref(),
            Some("1.20.30")
        );
        assert_eq!(version_in("# nothing here"), None);
        assert_eq!(version_in("# bun oven"), None);
    }

    #[test]
    fn the_committed_fixtures_agree_on_one_engine() {
        let fixtures = crate::repo_root()
            .unwrap()
            .join("crates/edm-core/tests/fixtures");
        assert!(recorded_version(&fixtures).unwrap().is_some());
    }
}
