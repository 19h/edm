//! The differential runner: the same argv through both implementations, against
//! the same mock, byte-diffed.
//!
//! This is the definition of done for the port. Every other test in this
//! repository asserts that a function does what its author believed the
//! TypeScript does; only this one asks the TypeScript.
//!
//! Five things are compared: stdout, stderr, the exit code, any `--dump` file,
//! and the wire log. Nothing is normalised except three named things, each of
//! which is a registered divergence or a harness artefact:
//!
//! * the mock's origin is mapped back to the production origin on the Rust
//!   side, because the TypeScript prints `API_ORIGIN` (ts:1181) while its
//!   `fetch` is redirected, and the Rust prints `EDM_ORIGIN_OVERRIDE` **[C24]**;
//! * the EDDN message timestamp, which `preload.ts` freezes on the Bun side and
//!   nothing freezes on the Rust side — its *shape* is still checked;
//! * injected request headers, per **[C19]** and **[C20]**, in `mock.rs`.
//!
//! One command has no oracle. `route` does not exist in the TypeScript **[C25]**,
//! so a `route` scenario declares `oracle = "none"` and is diffed against
//! committed goldens instead — a strictly weaker test, which is why
//! `scenario::validate` refuses that declaration on any other command.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::mock::{self, Mock};
use crate::scenario::{Oracle, Order, Profile, Scenario};

/// The credentials both sides run with.
///
/// Built here rather than stored in a scenario file on purpose: an 80- and a
/// 2024-character printable run committed to the tree is exactly what
/// `cargo xtask gates` scans for, and a harness that trips its own secret gate
/// teaches everyone to ignore it.
fn credentials() -> Vec<(&'static str, String)> {
    vec![
        ("COMMANDER_ID", "1234567".to_owned()),
        ("MACHINE_ID", "0123456789abcdef".to_owned()),
        ("MACHINE_TOKEN", "m".repeat(80)),
        ("AUTH_TOKEN", "a".repeat(2024)),
    ]
}

/// The stamp, pinned through the original's own environment fallbacks so that
/// argv stays exactly what the scenario declared.
///
/// With these three fixed the envelope plaintext is identical on both sides, so
/// the sealed query is identical, so the request line is comparable byte for
/// byte. **[R64]**
fn stamp() -> Vec<(&'static str, String)> {
    vec![
        ("NONCE", "0123456789ab".to_owned()),
        ("F_TIME", "1700000000".to_owned()),
        ("REQUEST_TIME", "86400000".to_owned()),
    ]
}

/// The instant `preload.ts` freezes `Date` at: 2023-11-14T22:13:20.000Z, the
/// same moment as `F_TIME`.
const FROZEN_NOW_MS: &str = "1700000000000";

/// The two sides of the differential diff, as they are named in a report.
const DIFFERENTIAL: [&str; 2] = ["bun", "rust"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Suite {
    All,
    /// Scenarios that make no requests — fast, and needs no server.
    Cli,
}

#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) suite: Suite,
    pub(crate) filter: Option<String>,
    pub(crate) list: bool,
}

#[derive(Debug)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
    dump: Option<Vec<u8>>,
    wire: String,
    timed_out: bool,
    /// Where this side ran. `--dump` writes into it and the program then
    /// *prints the path*, so the two sides necessarily disagree about a string
    /// that has nothing to do with the port.
    dir: String,
}

pub(crate) fn run(options: &Options) -> Result<()> {
    let root = crate::repo_root()?;
    let scenarios = crate::scenario::load_all(&root.join("xtask").join("scenarios"))?;

    let selected: Vec<&Scenario> = scenarios
        .iter()
        .filter(|scenario| options.suite != Suite::Cli || !scenario.network)
        .filter(|scenario| {
            options.filter.as_ref().is_none_or(|needle| scenario.name.contains(needle.as_str()))
        })
        .collect();
    if selected.is_empty() {
        bail!("no scenarios matched");
    }
    if options.list {
        for scenario in &selected {
            println!("{:<34} {}", scenario.name, scenario.why);
        }
        return Ok(());
    }

    // A golden-only selection (`--filter route`) has nothing to ask Bun, so it
    // must not require Bun to be installed.
    let bun = if selected.iter().any(|scenario| scenario.oracle == Oracle::Bun) {
        Some(which("bun").context(
            "`bun` is not on PATH — the parity harness has nothing to measure against without it",
        )?)
    } else {
        None
    };
    let binary = build_rust_binary(&root)?;
    let mock = if selected.iter().any(|scenario| scenario.network) {
        Some(Mock::start()?)
    } else {
        None
    };
    // A port nothing is listening on: the `cli` suite must not reach a socket,
    // and if a scenario is mis-declared the connection failure will say so.
    let base = mock.as_ref().map_or_else(|| "http://127.0.0.1:1".to_owned(), Mock::base_url);

    let work = root.join("target").join("xtask-parity");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    let mut failures = Vec::new();
    let mut passes = 0usize;
    for scenario in &selected {
        let started = Instant::now();
        let mut report =
            compare(scenario, &root, &work, bun.as_deref(), &binary, mock.as_ref(), &base)?;
        // A registered divergence is asserted, not ignored: the row in
        // PORTING.md claims the two sides differ here, and a row that has
        // quietly become true again is a row that should be deleted.
        if let Some(row) = &scenario.divergence {
            report = if report.is_empty() {
                vec![format!(
                    "{row} says this scenario diverges, and the two sides now agree — \
                     the register row is stale"
                )]
            } else {
                println!("        {row} holds: {}", report.join(" / ").replace('\n', " "));
                Vec::new()
            };
        }
        let elapsed = started.elapsed().as_millis();
        if report.is_empty() {
            passes += 1;
            println!("  pass  {:<34} {elapsed:>6} ms", scenario.name);
        } else {
            println!("  FAIL  {:<34} {elapsed:>6} ms", scenario.name);
            failures.push((scenario.file.clone(), report));
        }
    }

    println!();
    for (file, report) in &failures {
        println!("─── {} ───", file.display());
        for line in report {
            println!("{line}");
        }
        println!();
    }
    println!("{passes} passed, {} failed, {} total", failures.len(), selected.len());
    if failures.is_empty() { Ok(()) } else { bail!("{} scenarios diverged", failures.len()) }
}

/// Runs one scenario on both sides and returns the differences, empty on parity.
fn compare(
    scenario: &Scenario,
    root: &Path,
    work: &Path,
    bun: Option<&Path>,
    binary: &Path,
    mock: Option<&Mock>,
    base: &str,
) -> Result<Vec<String>> {
    let ordered_wire = scenario.in_flight == 1 && scenario.order == Order::Ordered;
    let dir = work.join(&scenario.name);

    let mut problems = Vec::new();
    let bun_side = match scenario.oracle {
        Oracle::Bun => {
            let bun = bun.context("this scenario has an oracle but bun was never located")?;
            let (side, notes) = ask_the_oracle(scenario, root, &dir, bun, mock, base, ordered_wire)?;
            problems.extend(notes);
            Some(side)
        }
        // Nothing to ask: `route` is not a command the TypeScript has. The
        // goldens below carry what would otherwise be Bun's half of the diff.
        Oracle::None => None,
    };

    if let Some(mock) = mock {
        mock.load(scenario);
    }
    let rust_side =
        execute(scenario, root, &dir.join("rust"), Command::new(binary), |command| {
            command
                .env("EDM_ORIGIN_OVERRIDE", base)
                .env("EDM_ARDENT_BASE", format!("{base}/v2"))
                .env("EDM_EDDN_URL", format!("{base}/upload/"));
        })?;
    let rust_observed = observe(mock, ordered_wire, &dir.join("rust"))?;
    problems.extend(rust_observed.problems.into_iter().map(|note| format!("mock (rust): {note}")));
    // Asserted on the Rust side alone, and deliberately: pacing is C27, a
    // divergence. The original has no pacer at all, so requiring a minimum gap
    // of Bun would be asserting the opposite of what the register records.
    problems.extend(check_timing(
        scenario.expect_frontier_requests,
        scenario.expect_min_gap_ms,
        &rust_observed.arrivals,
    ));

    if !scenario.network && mock.is_some_and(|mock| mock.request_count() > 0) {
        problems.push("declared `network = false` but a request reached the mock".to_owned());
    }

    let rust_side = Capture { wire: rust_observed.wire, ..rust_side };

    let mut report = problems;
    if rust_side.timed_out {
        report.push(format!("rust hit the {} s wall-clock limit", scenario.wall_clock_limit));
    }

    let Some(bun_side) = bun_side else {
        report.extend(against_goldens(scenario, root, &rust_side, base)?);
        return Ok(report);
    };
    if bun_side.timed_out {
        report.push(format!("bun hit the {} s wall-clock limit", scenario.wall_clock_limit));
    }

    if scenario.record_r86 {
        record_r86(root, &bun_side)?;
    }

    let multiset = scenario.order == Order::Multiset;
    if bun_side.code != rust_side.code {
        report.push(format!("exit code: bun {} vs rust {}", bun_side.code, rust_side.code));
    }
    let bun_stdout = normalise_side_dir(&bun_side.stdout, &bun_side.dir);
    let bun_stderr = normalise_side_dir(&bun_side.stderr, &bun_side.dir);
    let rust_stdout =
        normalise_side_dir(&canonicalise(&rust_side.stdout, base), &rust_side.dir);
    let rust_stderr =
        normalise_side_dir(&canonicalise(&rust_side.stderr, base), &rust_side.dir);
    if let Some(diff) = compare_stream("stdout", &bun_stdout, &rust_stdout, DIFFERENTIAL, multiset) {
        report.push(diff);
    }
    if let Some(diff) = compare_stream("stderr", &bun_stderr, &rust_stderr, DIFFERENTIAL, multiset) {
        report.push(diff);
    }
    match (&bun_side.dump, &rust_side.dump) {
        (Some(left), Some(right)) => {
            let right = canonicalise(right, base);
            if let Some(diff) = compare_stream("dump", left, &right, DIFFERENTIAL, false) {
                report.push(diff);
            }
        }
        (None, None) => {}
        (left, right) => report.push(format!(
            "dump file: bun {} vs rust {}",
            if left.is_some() { "written" } else { "absent" },
            if right.is_some() { "written" } else { "absent" },
        )),
    }
    let bun_wire = normalise_timestamps(&bun_side.wire);
    let rust_wire = normalise_timestamps(&String::from_utf8_lossy(&canonicalise(
        rust_side.wire.as_bytes(),
        base,
    )));
    let wire = compare_stream(
        "wire",
        bun_wire.as_bytes(),
        rust_wire.as_bytes(),
        DIFFERENTIAL,
        multiset,
    );
    if let Some(diff) = wire {
        report.push(diff);
    }
    Ok(report)
}

/// Runs the TypeScript under Bun and drains the mock into the Bun artefacts.
fn ask_the_oracle(
    scenario: &Scenario,
    root: &Path,
    dir: &Path,
    bun: &Path,
    mock: Option<&Mock>,
    base: &str,
    ordered_wire: bool,
) -> Result<(Capture, Vec<String>)> {
    if let Some(mock) = mock {
        mock.load(scenario);
    }
    let mut command = Command::new(bun);
    command
        // `env_clear` below scrubs the *shell*. It does not scrub the disk:
        // Bun loads `.env` from its working directory before the script runs,
        // and this harness runs in the repository root. A developer's `.env`
        // holding a live `MARKET_ID` therefore reached the Bun side and not the
        // Rust side, which is a divergence the harness invents rather than
        // finds — and a live `AUTH_TOKEN` reaching a side of a *differential*
        // test is worse than a false failure. Measured: `--env-file` replaces
        // the default set outright, so naming an empty file loads nothing.
        .arg("--env-file")
        .arg(root.join("xtask").join("oracle").join("empty.env"))
        .arg("--preload")
        .arg(root.join("xtask").join("oracle").join("preload.ts"))
        .arg(root.join("market-request.ts"));
    let side = execute(scenario, root, &dir.join("bun"), command, |command| {
        // The original `import()`s this module at run time (C1). Lossy, like
        // every other environment read in this program. **[R55]**
        let ardent = std::env::var_os("ARDENT_MODULE").map_or_else(
            || "/models/dev/edtrade/src/ardent.ts".to_owned(),
            |value| value.to_string_lossy().into_owned(),
        );
        command
            .env("EDM_MOCK_BASE", base)
            .env("EDM_MOCK_NOW", FROZEN_NOW_MS)
            .env("ARDENT_MODULE", ardent);
    })?;
    let observed = observe(mock, ordered_wire, &dir.join("bun"))?;
    let notes = observed.problems.into_iter().map(|note| format!("mock (bun): {note}")).collect();
    Ok((Capture { wire: observed.wire, ..side }, notes))
}

/// Drains the mock into this side's artefacts.
///
/// Both files are kept on disk beside the captured streams: when a wire diff
/// fires, the thing a human needs is the two logs side by side, and when a
/// pacing assertion fires it is the arrival instants.
fn observe(mock: Option<&Mock>, ordered: bool, dir: &Path) -> Result<mock::Observed> {
    let observed = mock.map_or_else(mock::Observed::default, |mock| mock.observe(ordered));
    std::fs::write(dir.join("wire.txt"), &observed.wire)?;
    std::fs::write(dir.join("timing.txt"), mock::format_timing(&observed.arrivals))?;
    Ok(observed)
}

/// The two assertions the wire log cannot make.
///
/// It records *what* was sent, and nothing in it distinguishes four requests
/// spread over a second from four sent at once, nor an empty log produced by a
/// spend gate that refused from one produced by a scenario that quietly stopped
/// reaching the gate at all.
fn check_timing(
    expect_requests: Option<usize>,
    expect_gap_ms: Option<u64>,
    arrivals: &[mock::Arrival],
) -> Vec<String> {
    let mut out = Vec::new();
    let seen = mock::frontier_count(arrivals);
    if let Some(expected) = expect_requests
        && seen != expected
    {
        out.push(format!(
            "expected exactly {expected} Companion API request(s); the rust side made {seen}"
        ));
    }
    if let Some(floor) = expect_gap_ms {
        match mock::min_frontier_gap(arrivals) {
            Some(gap) if gap < u128::from(floor) => out.push(format!(
                "expected at least {floor} ms between Companion API requests; the \
                 closest pair arrived {gap} ms apart"
            )),
            Some(_) => {}
            // A pacing assertion that passes because nothing was sent is the
            // failure mode this whole key exists to avoid.
            None => out.push(format!(
                "`expect-min-gap-ms = {floor}` has no gap to measure: {seen} Companion \
                 API request(s) arrived"
            )),
        }
    }
    out
}

/// Diffs the Rust side against the committed goldens, for the one command with
/// no oracle \[C25\].
///
/// The streams get exactly the normalisation the differential path applies to
/// this side, so a golden survives a change of ephemeral port and of working
/// directory. The wire log is written for inspection but not compared:
/// `expect-frontier-requests` is what makes a route scenario's request count
/// assertable, and it says so as a number rather than as a blob.
fn against_goldens(
    scenario: &Scenario,
    root: &Path,
    rust: &Capture,
    base: &str,
) -> Result<Vec<String>> {
    let dir = golden_dir(root, &scenario.name);
    if !dir.join("exit").exists() {
        return Ok(vec![format!(
            "no goldens under {} — run `cargo xtask bless --golden`",
            dir.display()
        )]);
    }
    let mut report = Vec::new();
    for (label, actual) in golden_streams(rust, base) {
        let expected = std::fs::read(dir.join(label))
            .with_context(|| format!("reading the {label} golden"))?;
        if let Some(diff) = compare_stream(label, &expected, &actual, ["golden", "rust"], false)
        {
            report.push(diff);
        }
    }
    let expected = std::fs::read_to_string(dir.join("exit"))?;
    let expected: i32 = expected
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .parse()
        .context("the `exit` golden is not an integer")?;
    if expected != rust.code {
        report.push(format!("exit code (golden): expected {expected}, got {}", rust.code));
    }
    if !report.is_empty() {
        report.push("re-bless with `cargo xtask bless --golden` if this is intended".to_owned());
    }
    Ok(report)
}

/// Records what the Rust side does now as the goldens for every scenario that
/// has no oracle.
///
/// The engine check that guards this lives in [`crate::bless`]; by the time
/// this runs, the decision to overwrite has been taken.
pub(crate) fn bless_goldens(root: &Path) -> Result<Vec<String>> {
    let scenarios = crate::scenario::load_all(&root.join("xtask").join("scenarios"))?;
    let selected: Vec<&Scenario> =
        scenarios.iter().filter(|scenario| scenario.oracle == Oracle::None).collect();
    if selected.is_empty() {
        bail!("no scenario declares `oracle = \"none\"`, so there is nothing to bless");
    }

    let binary = build_rust_binary(root)?;
    let mock = selected.iter().any(|scenario| scenario.network).then(Mock::start).transpose()?;
    let base = mock.as_ref().map_or_else(|| "http://127.0.0.1:1".to_owned(), Mock::base_url);
    let work = root.join("target").join("xtask-golden");
    let _ = std::fs::remove_dir_all(&work);

    let mut written = Vec::with_capacity(selected.len());
    for scenario in selected {
        if let Some(mock) = &mock {
            mock.load(scenario);
        }
        let dir = work.join(&scenario.name);
        let capture = execute(scenario, root, &dir, Command::new(&binary), |command| {
            command
                .env("EDM_ORIGIN_OVERRIDE", &base)
                .env("EDM_ARDENT_BASE", format!("{base}/v2"))
                .env("EDM_EDDN_URL", format!("{base}/upload/"));
        })?;
        // A killed process has no output worth committing, and a golden made
        // from one would turn a hang into a green.
        if capture.timed_out {
            bail!(
                "{} hit the {} s wall-clock limit; refusing to bless a killed run",
                scenario.name,
                scenario.wall_clock_limit
            );
        }
        let ordered = scenario.in_flight == 1 && scenario.order == Order::Ordered;
        let _ = observe(mock.as_ref(), ordered, &dir)?;

        let out = golden_dir(root, &scenario.name);
        std::fs::create_dir_all(&out)?;
        for (label, bytes) in golden_streams(&capture, &base) {
            std::fs::write(out.join(label), bytes)?;
        }
        std::fs::write(out.join("exit"), format!("{}\n", capture.code))?;
        written.push(scenario.name.clone());
    }
    Ok(written)
}

/// The streams a golden holds, normalised the way the differential path
/// normalises this side.
fn golden_streams(rust: &Capture, base: &str) -> [(&'static str, Vec<u8>); 2] {
    let clean = |bytes: &[u8]| {
        normalise_elapsed(&normalise_side_dir(&canonicalise(bytes, base), &rust.dir))
    };
    [("stdout", clean(&rust.stdout)), ("stderr", clean(&rust.stderr))]
}

/// Masks the coverage table's `elapsed` value.
///
/// It is a wall-clock measurement of the run itself, so it is the one cell in
/// that table that a byte-identical rerun can legitimately change — a retry
/// scenario that lands on either side of a second flips it. The *row* is still
/// asserted, and so is the shape of its value: anything that is not a duration
/// this program formats is left in place and will diff, which keeps the
/// rendering under test while the number is not.
fn normalise_elapsed(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else { return bytes.to_vec() };
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index > 0 {
            // `split_inclusive` keeps the terminator, so nothing is re-added.
        }
        if let Some((head, value, tail)) = elapsed_seconds_field(line) {
            out.push_str(head);
            out.push_str("<ELAPSED>");
            out.push_str(tail);
            let _ = value;
            continue;
        }
        match elapsed_cell(line) {
            Some((head, value, tail)) => {
                out.push_str(head);
                out.push_str("<ELAPSED>");
                // Keep the column width, so the frame still diffs if it moves.
                for _ in 0..value.len().saturating_sub("<ELAPSED>".len()) {
                    out.push(' ');
                }
                out.push_str(tail);
            }
            None => out.push_str(line),
        }
    }
    out.into_bytes()
}

/// `  "elapsedSeconds": 0.128,` split into the parts around its value.
///
/// The JSON half of the same problem the table has: a wall-clock measurement
/// of the run cannot be a golden. The key and the trailing comma still diff.
fn elapsed_seconds_field(line: &str) -> Option<(&str, &str, &str)> {
    const KEY: &str = "\"elapsedSeconds\":";
    let at = line.find(KEY)? + KEY.len();
    let rest = &line[at..];
    let end = rest.find([',', '\n', '}']).unwrap_or(rest.len());
    let value = rest[..end].trim_ascii();
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
        return None;
    }
    Some((&line[..at], &line[at..at + end], &line[at + end..]))
}

/// `| elapsed        | 4s           |` split into the parts around its value.
fn elapsed_cell(line: &str) -> Option<(&str, &str, &str)> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    let open = trimmed.find('|')?;
    let middle = trimmed[open + 1..].find('|')? + open + 1;
    // The whole label cell, not a prefix: `| elapsed since |` is a different
    // row and must diff like any other.
    if trimmed[open + 1..middle].trim_matches(' ') != "elapsed" {
        return None;
    }
    let close = trimmed[middle + 1..].find('|')? + middle + 1;
    let cell = &trimmed[middle + 1..close];
    let value = cell.trim_matches(' ');
    // Only a duration this program formats: `4s`, `2m 5s`, `1h 30m`.
    let formatted = value
        .split(' ')
        .all(|part| part.ends_with(['s', 'm', 'h']) && part[..part.len() - 1].chars().all(|c| c.is_ascii_digit() || c == ','));
    if value.is_empty() || !formatted {
        return None;
    }
    let head_len = middle + 2;
    Some((&line[..head_len], &line[head_len..close], &line[close..]))
}

fn golden_dir(root: &Path, name: &str) -> PathBuf {
    root.join("xtask").join("scenarios").join("golden").join(name)
}

/// Spawns one side, with the environment scrubbed down to what the scenario
/// declares.
///
/// `env_clear` is not caution, it is correctness: `MARKET_ID` or `AUTH_TOKEN`
/// in the developer's shell would silently change what the program does, and
/// **[R55]** makes the environment snapshot observable.
fn execute(
    scenario: &Scenario,
    root: &Path,
    dir: &Path,
    mut command: Command,
    configure: impl FnOnce(&mut Command),
) -> Result<Capture> {
    std::fs::create_dir_all(dir)?;
    let dump_path = dir.join("dump.out");
    let stdout_path = dir.join("stdout");
    let stderr_path = dir.join("stderr");

    for token in &scenario.argv {
        let token = if token == "{dump}" { dump_path.as_os_str().to_owned() } else { OsString::from(token) };
        command.arg(token);
    }

    command.current_dir(root).env_clear();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("COLUMNS", &scenario.columns);
    // A cache directory of this side's own.
    //
    // `route` caches market listings under `$XDG_CACHE_HOME`, and without this
    // every scenario would read and write the developer's real `~/.cache` —
    // so one scenario's sweep would silently change what the next one sends.
    // It was found the honest way: `route-ceiling-refuses` began proceeding
    // past a ceiling because an earlier scenario had already cached one of the
    // two markets it was counting. A test suite that writes to a home
    // directory is also just bad manners.
    let cache = dir.join("cache");
    std::fs::create_dir_all(&cache)?;
    command.env("XDG_CACHE_HOME", &cache);
    for (name, value) in credentials().into_iter().chain(stamp()) {
        command.env(name, value);
    }
    configure(&mut command);
    for (name, value) in &scenario.env {
        command.env(name, value);
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(std::fs::File::create(&stderr_path)?));

    let mut child = command.spawn().context("spawning a side of the comparison")?;
    let deadline = Instant::now() + Duration::from_secs(scenario.wall_clock_limit);
    let (code, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status.code().unwrap_or(-1), false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break (-1, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    Ok(Capture {
        stdout: std::fs::read(&stdout_path)?,
        stderr: std::fs::read(&stderr_path)?,
        code,
        dump: scenario.dump.then(|| std::fs::read(&dump_path).ok()).flatten(),
        wire: String::new(),
        timed_out,
        dir: dir.to_string_lossy().into_owned(),
    })
}

/// Maps the mock back onto the origins the production build would have used.
///
/// The TypeScript prints `API_ORIGIN` while fetching somewhere else, so its
/// output already reads as production. The Rust takes its origin from the
/// environment **[C24]** and prints what it was given. Rewriting the Rust side
/// is a narrower change than rewriting the TypeScript's constants would be, and
/// it is unambiguous: the three profiles occupy disjoint path prefixes.
fn canonicalise(bytes: &[u8], base: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else { return bytes.to_vec() };
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(base) {
        out.push_str(&rest[..index]);
        let tail = &rest[index + base.len()..];
        match Profile::of_path(tail) {
            Some(profile) => out.push_str(profile.production_origin()),
            // Not a path we route: leave the text exactly as it was rather than
            // inventing an origin for it.
            None => out.push_str(base),
        }
        rest = tail;
    }
    out.push_str(rest);
    out.into_bytes()
}

/// Replaces the side's own working directory with a placeholder.
///
/// The only thing that reaches it is `markets --dump <file>`, which reports the
/// path it wrote to. Each side writes into its own directory, so that one line
/// differs for a reason that is entirely the harness's doing. Everything else
/// about the dump — its contents, its reported length — is still diffed.
fn normalise_side_dir(bytes: &[u8], dir: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else { return bytes.to_vec() };
    text.replace(dir, "<OUTDIR>").into_bytes()
}

/// Replaces a well-formed EDDN message timestamp with a placeholder.
///
/// `preload.ts` freezes `Date` on the Bun side; nothing freezes the Rust side's
/// clock, because the program exposes no override for it. The *shape* is still
/// asserted — anything that is not `YYYY-MM-DDTHH:MM:SS.mmmZ` is left in place
/// and will diff, which keeps **[R20]** under test.
fn normalise_timestamps(text: &str) -> String {
    const KEY: &str = "\"timestamp\":\"";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(KEY) {
        out.push_str(&rest[..index + KEY.len()]);
        rest = &rest[index + KEY.len()..];
        let Some(end) = rest.find('"') else { break };
        let value = &rest[..end];
        if is_iso_instant(value) {
            out.push_str("<TIMESTAMP>");
        } else {
            out.push_str(value);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn is_iso_instant(value: &str) -> bool {
    let shape = b"0000-00-00T00:00:00.000Z";
    value.len() == shape.len()
        && value.bytes().zip(shape).all(|(byte, pattern)| match pattern {
            b'0' => byte.is_ascii_digit(),
            other => byte == *other,
        })
}

/// Diffs two streams, naming which side each line came from.
///
/// `sides` is a parameter rather than a constant because the left-hand side is
/// not always Bun: a scenario with no oracle reads it off disk, and a report
/// that called a golden `bun` would send its reader looking for a Bun run that
/// never happened.
fn compare_stream(
    label: &str,
    left: &[u8],
    right: &[u8],
    sides: [&str; 2],
    multiset: bool,
) -> Option<String> {
    let width = sides[0].len().max(sides[1].len());
    if multiset {
        let bag = |bytes: &[u8]| -> Vec<String> {
            let mut lines: Vec<String> =
                String::from_utf8_lossy(bytes).lines().map(str::to_owned).collect();
            lines.sort();
            lines
        };
        let (left, right) = (bag(left), bag(right));
        if left == right {
            return None;
        }
        let only_left: BTreeSet<&String> = left.iter().filter(|l| !right.contains(l)).collect();
        let only_right: BTreeSet<&String> = right.iter().filter(|l| !left.contains(l)).collect();
        let mut out = format!("{label} (as a multiset) differs:");
        for line in only_left.iter().take(6) {
            let _ = write!(out, "\n    {:<width$} only: {line}", sides[0]);
        }
        for line in only_right.iter().take(6) {
            let _ = write!(out, "\n    {:<width$} only: {line}", sides[1]);
        }
        return Some(out);
    }
    if left == right {
        return None;
    }
    let left_lines: Vec<&str> = std::str::from_utf8(left).unwrap_or("<non-UTF-8>").lines().collect();
    let right_lines: Vec<&str> =
        std::str::from_utf8(right).unwrap_or("<non-UTF-8>").lines().collect();
    let first = left_lines
        .iter()
        .zip(&right_lines)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| left_lines.len().min(right_lines.len()));
    let mut out = format!(
        "{label} differs at line {} ({} vs {} lines):",
        first + 1,
        left_lines.len(),
        right_lines.len()
    );
    for offset in 0..4 {
        let index = first + offset;
        match (left_lines.get(index), right_lines.get(index)) {
            (None, None) => break,
            (left, right) => {
                let number = index + 1;
                let _ =
                    write!(out, "\n    {:<width$} {number}| {}", sides[0], left.unwrap_or(&"<eof>"));
                let _ = write!(
                    out,
                    "\n    {:<width$} {number}| {}",
                    sides[1],
                    right.unwrap_or(&"<eof>")
                );
            }
        }
    }
    Some(out)
}

/// **[R86]** — the one row in the register that is a measurement rather than a
/// transcription.
///
/// `withTimeout` (ts:1442) aborts the controller and *then* rejects with
/// `timed out after {n} ms`, and the fetch it aborted rejects with an
/// `AbortError` that `describeFailure` renders as `aborted (timeout)`. Which of
/// the two wins `Promise.race` is a question about microtask ordering that no
/// amount of reading settles. So we ask.
fn record_r86(root: &Path, bun: &Capture) -> Result<()> {
    let text = String::from_utf8_lossy(&bun.stdout);
    let observed = text
        .lines()
        .chain(String::from_utf8_lossy(&bun.stderr).lines().collect::<Vec<_>>())
        .find_map(|line| {
            if line.contains("aborted (timeout)") {
                Some("aborted (timeout)")
            } else if line.contains("timed out after") {
                Some("timed out after {n} ms")
            } else {
                None
            }
        })
        .unwrap_or("<neither wording appeared>");

    let path = root.join("xtask").join("fixtures").join("r86-timeout-wording.txt");
    std::fs::create_dir_all(path.parent().unwrap_or(root))?;
    std::fs::write(
        &path,
        format!(
            "# R86 — which wording a sweep prints when an attempt times out.\n\
             # Measured by `cargo xtask parity --filter r86`; do not hand-edit.\n\
             # `edm::sweep::timeout_failure` must produce this.\n\
             {observed}\n"
        ),
    )?;
    Ok(())
}

fn build_rust_binary(root: &Path) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("EDM_BIN") {
        return Ok(PathBuf::from(path));
    }
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["build", "-p", "edm"])
        .current_dir(root)
        .status()
        .context("running cargo build -p edm")?;
    if !status.success() {
        bail!("cargo build -p edm failed");
    }
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| root.join("target"), PathBuf::from);
    let binary = target.join("debug").join("edm");
    if !binary.exists() {
        bail!("built, but {} is missing", binary.display());
    }
    Ok(binary)
}

fn which(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is unset")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("{program} not found on PATH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mock_origin_maps_back_to_the_three_production_origins() {
        let base = "http://127.0.0.1:4321";
        let input = format!(
            "endpoint {base}/2.0/elite/market/list?abc\nardent {base}/v2/system/name/Sol\n\
             eddn {base}/upload/\nunrouted {base}/nope\n"
        );
        let out = String::from_utf8(canonicalise(input.as_bytes(), base)).unwrap();
        assert!(out.contains("https://api.orerve.net/2.0/elite/market/list?abc"), "{out}");
        assert!(out.contains("https://api.ardent-insight.com/v2/system/name/Sol"), "{out}");
        assert!(out.contains("https://eddn.edcd.io:4430/upload/"), "{out}");
        assert!(out.contains("unrouted http://127.0.0.1:4321/nope"), "{out}");
    }

    #[test]
    fn only_well_formed_timestamps_are_normalised() {
        assert_eq!(
            normalise_timestamps(r#"{"timestamp":"2023-11-14T22:13:20.000Z"}"#),
            r#"{"timestamp":"<TIMESTAMP>"}"#
        );
        // A malformed instant is left alone so that it diffs; R20 stays live.
        assert_eq!(
            normalise_timestamps(r#"{"timestamp":"2023-11-14T22:13:20Z"}"#),
            r#"{"timestamp":"2023-11-14T22:13:20Z"}"#
        );
    }

    #[test]
    fn an_ordered_diff_names_the_first_differing_line() {
        let report =
            compare_stream("stdout", b"a\nb\nc\n", b"a\nB\nc\n", DIFFERENTIAL, false).unwrap();
        assert!(report.starts_with("stdout differs at line 2"), "{report}");
    }

    #[test]
    fn a_multiset_diff_ignores_order_but_not_content() {
        assert!(compare_stream("stdout", b"a\nb\n", b"b\na\n", DIFFERENTIAL, true).is_none());
        assert!(compare_stream("stdout", b"a\nb\n", b"b\nc\n", DIFFERENTIAL, true).is_some());
    }

    fn arrivals(millis: &[u128]) -> Vec<mock::Arrival> {
        millis
            .iter()
            .map(|millis| mock::Arrival { profile: Profile::Frontier, millis: *millis })
            .collect()
    }

    #[test]
    fn an_exact_request_count_reports_both_directions() {
        assert!(check_timing(Some(0), None, &[]).is_empty());
        let too_many = check_timing(Some(0), None, &arrivals(&[3])).remove(0);
        assert!(too_many.contains("exactly 0"), "{too_many}");
        let too_few = check_timing(Some(2), None, &arrivals(&[3])).remove(0);
        assert!(too_few.contains("made 1"), "{too_few}");
    }

    #[test]
    fn a_pacing_assertion_with_nothing_to_measure_fails() {
        // The failure this key exists to prevent: a scenario that stops sending
        // anything at all would otherwise satisfy every minimum gap there is.
        let empty = check_timing(None, Some(250), &[]).remove(0);
        assert!(empty.contains("no gap to measure"), "{empty}");
        assert!(check_timing(None, Some(250), &arrivals(&[0, 250, 600])).is_empty());
        let tight = check_timing(None, Some(250), &arrivals(&[0, 249])).remove(0);
        assert!(tight.contains("249 ms apart"), "{tight}");
    }

    #[test]
    fn the_credentials_are_the_lengths_the_original_validates() {
        let all = credentials();
        assert_eq!(all[2].1.len(), 80);
        assert_eq!(all[3].1.len(), 2024);
    }
}

#[cfg(test)]
mod elapsed_tests {
    use super::*;

    /// The one cell a byte-identical rerun may legitimately change.
    #[test]
    fn the_elapsed_value_is_masked_and_the_column_keeps_its_width() {
        let before = "| elapsed        | 4s           |\n";
        let after = String::from_utf8(normalise_elapsed(before.as_bytes())).expect("utf8");
        assert_eq!(after, "| elapsed        | <ELAPSED>    |\n");
        assert_eq!(after.len(), before.len(), "the frame must not move");
    }

    /// Only a duration this program formats. Anything else stays and diffs, so
    /// the rendering itself is still under test.
    #[test]
    fn a_value_that_is_not_a_duration_is_left_alone() {
        for line in [
            "| elapsed        | tomorrow     |\n",
            "| elapsed        |              |\n",
            "| elapsed since  | 4s           |\n",
            "| markets polled | 2 of 2       |\n",
        ] {
            let after = String::from_utf8(normalise_elapsed(line.as_bytes())).expect("utf8");
            assert_eq!(after, line, "{line}");
        }
    }

    /// Every duration shape `duration_estimate` produces.
    #[test]
    fn every_formatted_duration_is_recognised() {
        for value in ["4s", "59s", "2m 5s", "1h 30m", "1,200s"] {
            let line = format!("| elapsed        | {value:<12} |\n");
            let after = String::from_utf8(normalise_elapsed(line.as_bytes())).expect("utf8");
            assert!(after.contains("<ELAPSED>"), "{value}: {after}");
        }
    }
}
