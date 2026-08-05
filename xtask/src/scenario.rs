//! One scripted run of the program, on both implementations.
//!
//! A scenario is argv plus the exact bytes the three servers will answer with.
//! Everything that could otherwise vary between two runs of the same program is
//! nailed down here or in [`crate::parity`]: the nonce, `fTime`, `Request-Time`
//! and `COLUMNS` come from the original's own flags, and the response bodies
//! are built once and served identically to both sides.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::codec::{self, Encoding};
use crate::toml;

/// Which of the three servers a path belongs to.
///
/// One port serves all three; the path decides. `edm-mock` is one process
/// because the two clients are configured independently and a scenario has to
/// be able to see them interleave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Frontier,
    Ardent,
    Eddn,
}

impl Profile {
    pub(crate) fn of_path(path: &str) -> Option<Self> {
        if path.starts_with("/2.0/elite/") {
            Some(Self::Frontier)
        } else if path.starts_with("/v2/") {
            Some(Self::Ardent)
        } else if path.starts_with("/upload/") {
            Some(Self::Eddn)
        } else {
            None
        }
    }

    /// The origin the production build would have used, which is what the
    /// TypeScript prints even when its `fetch` has been redirected.
    pub(crate) fn production_origin(self) -> &'static str {
        match self {
            Self::Frontier => "https://api.orerve.net",
            Self::Ardent => "https://api.ardent-insight.com",
            Self::Eddn => "https://eddn.edcd.io:4430",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Frontier => "frontier",
            Self::Ardent => "ardent",
            Self::Eddn => "eddn",
        })
    }
}

/// How the two output streams are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Order {
    /// Byte-for-byte, in sequence. The default, and what every scenario should
    /// be unless it cannot.
    Ordered,
    /// As a bag of lines. **[C21]** — removing the original's 25 ms requeue
    /// poll changes the *order* in which a drained queue's stragglers finish.
    Multiset,
}

#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) reason: String,
    /// Literal header lines, in order, duplicates preserved: `Headers.get`
    /// joining duplicates with `", "` is **[R71]**, and a mock that used a map
    /// could not produce the two `uncompressedsize` headers that pins it.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) delay_ms: u64,
    /// Accept the request, log it, and never answer. The R86 measurement.
    pub(crate) never: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Route {
    pub(crate) path: String,
    /// A substring that must appear in the *decrypted* request envelope.
    /// Without it the seven markets of a sweep are indistinguishable, because
    /// the market id travels inside the encrypted query.
    pub(crate) envelope: Option<String>,
    /// A substring that must appear in the request body — how one EDDN post is
    /// told from another.
    pub(crate) body_contains: Option<String>,
    pub(crate) replies: Vec<Reply>,
}

#[derive(Clone, Debug)]
pub(crate) struct Scenario {
    pub(crate) name: String,
    pub(crate) file: PathBuf,
    /// What this scenario is for. Required: a scenario nobody can explain is a
    /// scenario nobody will maintain.
    pub(crate) why: String,
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
    /// `COLUMNS`, which pins `TERMINAL_WIDTH` on both sides. **[R31]**
    pub(crate) columns: String,
    /// False for the `cli` suite: no server is started and no request may be
    /// made.
    pub(crate) network: bool,
    pub(crate) order: Order,
    pub(crate) because: String,
    /// The scenario writes a `--dump` file, spelled `{dump}` in argv.
    pub(crate) dump: bool,
    /// Records the R86 timeout wording from the Bun side into a fixture.
    pub(crate) record_r86: bool,
    /// A register row from `PORTING.md`'s CORRECT table. The scenario then
    /// asserts the *opposite*: the two sides must differ, and agreeing means
    /// the row has gone stale and should be deleted.
    pub(crate) divergence: Option<String>,
    pub(crate) routes: Vec<Route>,
    /// How many requests the program can have outstanding at once, derived from
    /// argv rather than declared, so it cannot drift from what actually runs.
    pub(crate) in_flight: u32,
    pub(crate) wall_clock_limit: u64,
}

const SCENARIO_KEYS: &[&str] = &[
    "name",
    "why",
    "argv",
    "columns",
    "network",
    "order",
    "because",
    "dump",
    "record",
    "divergence",
    "wall-clock-limit",
];

/// The two paths a sweep worker touches.
///
/// Everything else — the star system read, the Ardent lookups — happens on the
/// main path before the pool exists, so a delay there cannot produce the tie
/// that [`validate`] is looking for.
const POOLED_PATHS: [&str; 2] = ["/2.0/elite/market/list", "/upload/"];
const REPLY_KEYS: &[&str] = &[
    "status",
    "reason",
    "headers",
    "delay-ms",
    "never",
    "nonce",
    "encode",
    "payload",
    "payload-file",
    "size-header",
    "omit-size",
    "omit-nonce",
    "gzip",
];

pub(crate) fn load_all(dir: &Path) -> Result<Vec<Scenario>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();

    let mut scenarios = Vec::with_capacity(files.len());
    for file in files {
        let scenario = load(&file).with_context(|| format!("in {}", file.display()))?;
        scenarios.push(scenario);
    }
    if scenarios.is_empty() {
        bail!("no scenarios found in {}", dir.display());
    }
    Ok(scenarios)
}

fn load(file: &Path) -> Result<Scenario> {
    let text = std::fs::read_to_string(file)?;
    let doc = toml::parse(&text)?;
    doc.reject_unknown(SCENARIO_KEYS, &["env", "route"])?;

    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
    let name = doc.string("name", &stem)?;
    let why = doc.required_string("why").context("every scenario must say what it is for")?;
    let argv = doc.str_array("argv")?;
    let network = doc.boolean("network", true)?;
    let order = match doc.string("order", "ordered")?.as_str() {
        "ordered" => Order::Ordered,
        "multiset" => Order::Multiset,
        other => bail!("`order` must be `ordered` or `multiset`, not `{other}`"),
    };
    let because = doc.string("because", "")?;
    let record_r86 = match doc.string("record", "")?.as_str() {
        "" => false,
        "r86" => true,
        other => bail!("`record` may only be `r86`, not `{other}`"),
    };

    let env = doc.table("env").map(toml::Table::string_pairs).transpose()?.unwrap_or_default();

    let mut routes = Vec::new();
    for table in doc.tables("route") {
        table.reject_unknown(&["path", "envelope", "body-contains"], &["reply"])?;
        let path = table.required_string("path")?;
        if Profile::of_path(&path).is_none() {
            bail!("`{path}` is not under /2.0/elite/, /v2/ or /upload/, so nothing routes to it");
        }
        let mut replies = Vec::new();
        for reply in table.tables("reply") {
            replies.push(build_reply(reply, file).with_context(|| format!("route `{path}`"))?);
        }
        if replies.is_empty() {
            bail!("route `{path}` has no replies");
        }
        routes.push(Route {
            path,
            envelope: table.str_opt("envelope")?.map(str::to_owned),
            body_contains: table.str_opt("body-contains")?.map(str::to_owned),
            replies,
        });
    }

    let scenario = Scenario {
        in_flight: in_flight(&argv),
        name,
        file: file.to_owned(),
        why,
        dump: argv.iter().any(|token| token == "{dump}"),
        argv,
        env,
        columns: doc.string("columns", "100")?,
        network,
        order,
        because,
        record_r86,
        divergence: doc.str_opt("divergence")?.map(str::to_owned),
        routes,
        wall_clock_limit: doc.int("wall-clock-limit", 60)?.try_into()?,
    };
    validate(&scenario)?;
    Ok(scenario)
}

fn build_reply(table: &toml::Table, file: &Path) -> Result<Reply> {
    table.reject_unknown(REPLY_KEYS, &[])?;
    let status: u16 = table.int("status", 200)?.try_into().context("`status`")?;

    // C3: our HTTP client reports `StatusCode::canonical_reason()` because
    // hyper discards the phrase off the wire. Scripting a non-canonical phrase
    // would therefore diff for a reason that has nothing to do with the port,
    // and — worse — a scenario could "pass" by having its real difference
    // hidden behind an expected one.
    let canonical = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason())
        .with_context(|| format!("status {status} has no canonical reason phrase"))?;
    let reason = table.string("reason", canonical)?;
    if reason != canonical {
        bail!(
            "reason phrase `{reason}` for status {status} is not the canonical \
             `{canonical}`; C3 constrains the mock to canonical phrases so that \
             it cannot mask a real difference"
        );
    }

    let encoding = Encoding::parse(&table.string("encode", "raw")?)?;
    let payload = match table.str_opt("payload-file")? {
        Some(relative) => {
            let path = file.parent().unwrap_or(Path::new(".")).join(relative);
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading payload {}", path.display()))?
        }
        None => table.string("payload", "")?,
    };
    let nonce = table.string("nonce", "0123456789ab")?;
    let sealed = codec::seal(&payload, encoding, &nonce)?;

    let mut headers = Vec::new();
    if encoding != Encoding::Raw {
        if !table.boolean("omit-nonce", false)? {
            headers.push(("Nonce".to_owned(), nonce));
        }
        if !table.boolean("omit-size", false)? {
            let declared = match table.str_opt("size-header")? {
                Some(literal) => literal.to_owned(),
                None => sealed
                    .uncompressed
                    .context("this encoding produces no size; set `size-header` or `omit-size`")?
                    .to_string(),
            };
            headers.push(("uncompressedsize".to_owned(), declared));
        }
    }
    headers.extend(table.pair_array("headers")?);

    let mut body = sealed.bytes;
    if table.boolean("gzip", false)? {
        body = codec::gzip(&body);
        headers.push(("Content-Encoding".to_owned(), "gzip".to_owned()));
    }

    Ok(Reply {
        status,
        reason,
        headers,
        body,
        delay_ms: table.int("delay-ms", 0)?.try_into().context("`delay-ms`")?,
        never: table.boolean("never", false)?,
    })
}

/// The most requests this argv can have outstanding at once.
///
/// Derived, never declared: a scenario that says `--concurrency 3` and claims
/// one in flight would switch off the tie check that makes its output
/// deterministic.
fn in_flight(argv: &[String]) -> u32 {
    let command = argv.iter().find(|token| !token.starts_with('-')).map(String::as_str);
    let sweeps = matches!(command, Some("market" | "list") | None)
        && !argv.iter().any(|token| token == "--market-id")
        && argv.iter().skip(1).any(|token| !token.starts_with('-'));
    if !sweeps {
        return 1;
    }
    let declared = argv
        .iter()
        .position(|token| token == "--concurrency")
        .and_then(|index| argv.get(index + 1))
        .and_then(|value| value.parse::<u32>().ok());
    // ts:1635 — `max(1, min(MAX_CONCURRENCY, n))`. **[R51]**
    declared.map_or(5, |n| n.clamp(1, 16))
}

fn validate(scenario: &Scenario) -> Result<()> {
    if scenario.order == Order::Multiset
        && scenario.because.chars().all(char::is_whitespace)
    {
        bail!(
            "`order = \"multiset\"` needs `because = \"…\"`: C21 says every \
             multiset comparison must carry its justification in the scenario file"
        );
    }
    if scenario.order == Order::Ordered && !scenario.because.is_empty() {
        bail!("`because` is only meaningful with `order = \"multiset\"`");
    }
    if !scenario.network && !scenario.routes.is_empty() {
        bail!("`network = false` scenarios run without a server, so they may not script routes");
    }
    if scenario.record_r86 && !scenario.routes.iter().flat_map(|r| &r.replies).any(|r| r.never) {
        bail!("the R86 measurement needs a route that never responds");
    }

    // No two scripted responses may share a completion instant. When more than
    // one request can be in flight, a tie is resolved by promise settlement
    // order in Bun and by worker index in Rust — the two orders are unrelated,
    // and the resulting line ordering would be a coin flip rather than a
    // measurement. Staggering the delays removes the coin flip; a scenario that
    // wants the tie has to say `order = "multiset"` and justify it.
    if let Some(row) = &scenario.divergence
        && !(row.starts_with('C') && row[1..].chars().all(|c| c.is_ascii_digit()))
    {
        bail!("`divergence` names a row in PORTING.md's CORRECT table, like `C11`, not `{row}`");
    }

    // Two replies on the *same* route are two attempts at the same market and
    // can never be outstanding together; the tie that matters is between
    // different targets, so distinctness is required across routes only.
    if scenario.in_flight > 1 && scenario.order == Order::Ordered {
        let mut seen: Vec<u64> = Vec::new();
        for route in
            scenario.routes.iter().filter(|route| POOLED_PATHS.contains(&route.path.as_str()))
        {
            let mut mine: Vec<u64> = Vec::new();
            for reply in route.replies.iter().filter(|reply| !reply.never) {
                if reply.delay_ms == 0 {
                    bail!(
                        "{} requests can be in flight, so every reply needs a distinct \
                         non-zero `delay-ms` (or `order = \"multiset\"`)",
                        scenario.in_flight
                    );
                }
                if seen.contains(&reply.delay_ms) {
                    bail!(
                        "two targets both complete at {} ms; ties resolve by promise \
                         settlement in Bun and by worker index in Rust, so the line \
                         order would be a coin flip",
                        reply.delay_ms
                    );
                }
                mine.push(reply.delay_ms);
            }
            seen.extend(mine);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_route_to_the_three_profiles() {
        assert_eq!(Profile::of_path("/2.0/elite/market/list"), Some(Profile::Frontier));
        assert_eq!(Profile::of_path("/v2/system/name/Sol"), Some(Profile::Ardent));
        assert_eq!(Profile::of_path("/upload/"), Some(Profile::Eddn));
        assert_eq!(Profile::of_path("/favicon.ico"), None);
    }

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| (*t).to_owned()).collect()
    }

    #[test]
    fn in_flight_follows_the_original_clamp() {
        assert_eq!(in_flight(&argv(&["market", "Colonia"])), 5);
        assert_eq!(in_flight(&argv(&["market", "Colonia", "--concurrency", "3"])), 3);
        assert_eq!(in_flight(&argv(&["market", "Colonia", "--concurrency", "0"])), 1);
        assert_eq!(in_flight(&argv(&["market", "Colonia", "--concurrency", "99"])), 16);
        assert_eq!(in_flight(&argv(&["market", "--market-id", "7"])), 1);
        assert_eq!(in_flight(&argv(&["markets", "Colonia"])), 1);
        assert_eq!(in_flight(&argv(&["trade", "--type", "buy"])), 1);
    }

    #[test]
    fn every_committed_scenario_loads_and_validates() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios");
        let scenarios = load_all(&dir).unwrap();
        assert!(scenarios.len() >= 20, "only {} scenarios", scenarios.len());
        let mut names: Vec<&str> = scenarios.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate scenario names");
    }
}
