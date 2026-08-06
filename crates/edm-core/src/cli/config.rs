//! Per-command configuration: what each command reads, and **when**.
//!
//! [`super::access`] can read any option at any time; this module decides which
//! options a command reads and in which order. That order is the whole point.
//! A command that fails on its fourth option has already performed the side
//! effects of the first three, and several of those side effects are network
//! requests — so `market --market-id 3 --cargo abc` must reach the market
//! listing before it ever complains about `--cargo` \[R50\].
//!
//! Nothing here is eager and nothing here is reordered for tidiness. Where the
//! TypeScript reads an option twice, this reads it twice; where it reads one
//! inside a conditional branch, this does too, because an option that is never
//! read can never be rejected. The functions are therefore small, ordered, and
//! deliberately not composed into a single "parse everything" entry point.
//!
//! The three ambient values a request stamp needs — entropy, the wall clock and
//! the process uptime — arrive as [`StampDefaults`], because this crate has
//! none of them.

use crate::consts::{
    AUTH_TOKEN_MIN_LENGTH, DEFAULT_CONCURRENCY, DEFAULT_REQUEUES, DEFAULT_TIMEOUT_SECONDS,
    MACHINE_TOKEN_LENGTH, MAX_CONCURRENCY,
};
use crate::domain::eddn::EddnOptions;
use crate::domain::trade::{self, Kind, Space, TradePlan};
use crate::domain::{self, Commodity, MarketSnapshot};
use crate::js::{self, text};
use crate::wire::Nonce;

use super::access::{Cli, CliError};
use super::flag::Flag;

// ---------------------------------------------------------------------------
// Credentials and the session
// ---------------------------------------------------------------------------

/// The four secrets every Companion API request carries (ts:46).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    pub commander_id: String,
    pub machine_id: String,
    pub machine_token: String,
    pub auth_token: String,
}

/// `validateAscii` (ts:60) — `/^[\x20-\x7e]+$/`.
///
/// The `+` matters: the pattern rejects the empty string. It is unreachable
/// from `loadCredentials`, whose `requireValue` never yields blank, but the
/// predicate is transcribed rather than simplified because the same helper
/// guards `--user-agent` elsewhere.
///
/// The regex carries no `u` flag, so it matches UTF-16 code units; every unit
/// in `\x20-\x7e` is also an ASCII byte, which makes a byte scan exact.
fn validate_ascii(name: &str, value: &str) -> Result<(), CliError> {
    if value.is_empty() || !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        // ts:61
        return Err(format!("{name} must contain printable ASCII only").into());
    }
    Ok(())
}

/// A floor rather than an exact length, for a value this program does not
/// issue \[C31\].
///
/// Measured in UTF-16 code units, like its exact sibling \[R22\].
fn validate_min_length(name: &str, value: &str, minimum: usize) -> Result<(), CliError> {
    let received = text::utf16_len(value);
    if received < minimum {
        return Err(format!(
            "{name} looks truncated: {received} characters, expected at least {minimum}"
        )
        .into());
    }
    Ok(())
}

/// `validateExactLength` (ts:64) — measured in UTF-16 code units \[R22\].
fn validate_exact_length(name: &str, value: &str, expected: usize) -> Result<(), CliError> {
    let received = text::utf16_len(value);
    if received != expected {
        // ts:66
        return Err(
            format!("{name} must be exactly {expected} characters; received {received}").into()
        );
    }
    Ok(())
}

/// `loadCredentials` (ts:75).
///
/// The field order is the object literal's, and within the two token fields the
/// ASCII test runs before the length test because `validateExactLength` wraps
/// `validateAscii`'s *result* \[R50\]. Reproducing that nesting is what makes an
/// 80-character token containing a tab report "printable ASCII" rather than a
/// length. The names in the messages are the TypeScript's camelCase field
/// names, not flag spellings.
pub fn load_credentials(cli: &Cli<'_>) -> Result<Credentials, CliError> {
    let commander_id = cli.require_value(Flag::CmdrId, Some("COMMANDER_ID"))?;
    validate_ascii("cmdrId", commander_id)?;

    let machine_id = cli.require_value(Flag::MachineId, Some("MACHINE_ID"))?;
    validate_ascii("machineId", machine_id)?;

    let machine_token = cli.require_value(Flag::MachineToken, Some("MACHINE_TOKEN"))?;
    validate_ascii("machineToken", machine_token)?;
    validate_exact_length("machineToken", machine_token, MACHINE_TOKEN_LENGTH)?;

    let auth_token = cli.require_value(Flag::AuthToken, Some("AUTH_TOKEN"))?;
    validate_ascii("authToken", auth_token)?;
    validate_min_length("authToken", auth_token, AUTH_TOKEN_MIN_LENGTH)?;

    Ok(Credentials {
        commander_id: commander_id.to_owned(),
        machine_id: machine_id.to_owned(),
        machine_token: machine_token.to_owned(),
        auth_token: auth_token.to_owned(),
    })
}

/// `Session` (ts:1123) minus the parsed arguments it carries around.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionConfig {
    pub credentials: Credentials,
    /// `--method`, uppercased; otherwise each endpoint uses its own verb.
    pub method_override: Option<String>,
    pub dry_run: bool,
    pub full_url: bool,
    pub json: bool,
}

/// `openSession` (ts:1133).
///
/// Credentials load first and load for *every* command, including
/// `markets --dry-run`, which never sends anything \[R50\]. A missing
/// `AUTH_TOKEN` therefore fails a dry run.
pub fn open_session(cli: &Cli<'_>) -> Result<SessionConfig, CliError> {
    Ok(SessionConfig {
        credentials: load_credentials(cli)?,
        // Full-Unicode uppercasing, as `String.prototype.toUpperCase` performs.
        method_override: cli.optional_value(Flag::Method, None).map(str::to_uppercase),
        dry_run: cli.switch_value(Flag::DryRun, false)?,
        full_url: cli.switch_value(Flag::FullUrl, false)?,
        json: cli.switch_value(Flag::Json, false)?,
    })
}

// ---------------------------------------------------------------------------
// The request stamp
// ---------------------------------------------------------------------------

/// The ambient values `nextStamp` would sample for itself.
///
/// `randomBytes(6)`, `Date.now()` and `uptime()`. They are parameters because
/// this crate has no entropy, no clock and no process. The arithmetic that
/// turns them into stamp fields stays here, since it is the part that is
/// observable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StampDefaults {
    /// `randomBytes(6)`, rendered as twelve lowercase hex characters.
    pub entropy: [u8; 6],
    /// `Date.now()` — milliseconds since the epoch.
    pub now_ms: f64,
    /// `uptime()` — seconds, fractional.
    pub uptime_seconds: f64,
}

/// `RequestStamp` (ts:94) — the values the game regenerates per request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RequestStamp {
    pub nonce: Nonce,
    pub frontier_time: f64,
    /// The game sends a wrapping 32-bit millisecond uptime.
    pub request_time: u32,
}

/// `nextStamp` (ts:100).
///
/// All three flags are read before any of them is validated, then the object
/// literal validates in field order — so a bad `--nonce` is reported before a
/// bad `--f-time` even though both were already fetched.
///
/// The names in the two `parseUnsignedInteger` messages are the literals
/// `fTime` and `requestTime`, which are neither the flag spellings \[R46\]
/// would give (`--f-time`, `--request-time`) nor the environment names.
///
/// `>>> 0` wraps modulo 2³², so `--request-time 4294967296` sends 0 \[R15\].
/// It is applied to the whole ternary, which means the uptime default is
/// wrapped too.
pub fn next_stamp(cli: &Cli<'_>, defaults: StampDefaults) -> Result<RequestStamp, CliError> {
    let nonce = cli.optional_value(Flag::Nonce, Some("NONCE"));
    let frontier_time = cli.optional_value(Flag::FTime, Some("F_TIME"));
    let request_time = cli.optional_value(Flag::RequestTime, Some("REQUEST_TIME"));

    let nonce = match nonce {
        // ts:73, via `NonceError`.
        Some(raw) => Nonce::parse_arg(raw).map_err(|error| CliError::from(error.to_string()))?,
        None => Nonce::from_entropy(defaults.entropy),
    };
    let frontier_time = match frontier_time {
        Some(raw) => js::parse_unsigned_integer("fTime", raw).map_err(CliError::from)?,
        None => (defaults.now_ms / 1_000.0).floor(),
    };
    let request_time = match request_time {
        Some(raw) => js::parse_unsigned_integer("requestTime", raw).map_err(CliError::from)?,
        None => (defaults.uptime_seconds * 1_000.0).floor(),
    };

    Ok(RequestStamp { nonce, frontier_time, request_time: js::to_uint32(request_time) })
}

// ---------------------------------------------------------------------------
// market
// ---------------------------------------------------------------------------

/// What `market` was pointed at.
#[derive(Clone, Debug, PartialEq)]
pub enum MarketTarget {
    /// One market, by parsed id.
    Single(f64),
    /// A whole system or station name, to be swept.
    Sweep(String),
}

/// `runMarket` (ts:1685).
///
/// The market-id staging is three-phase and each phase is load-bearing \[R52\].
/// The first read deliberately omits the `MARKET_ID` fallback, so an explicit
/// flag pins one market; then a name sweeps; and only if there is no name at
/// all does the environment get consulted. Collapsing the first and third reads
/// into one would stop `market Colonia` from sweeping whenever `MARKET_ID`
/// happens to be set — which is the case the staging exists for.
///
/// Name precedence here is `--system ?? --station ?? positional`, the
/// **opposite** of `markets` \[R52\].
pub fn market_target(cli: &Cli<'_>) -> Result<MarketTarget, CliError> {
    if let Some(explicit) = cli.optional_value(Flag::MarketId, None) {
        return js::parse_unsigned_integer("--market-id", explicit)
            .map(MarketTarget::Single)
            .map_err(CliError::from);
    }

    let positional = positional_name(cli);
    let name = cli
        .optional_value(Flag::System, None)
        .or_else(|| cli.optional_value(Flag::Station, None))
        .map(str::to_owned)
        .or(positional);
    if let Some(name) = name {
        return Ok(MarketTarget::Sweep(name));
    }

    let Some(from_environment) = cli.optional_value(Flag::MarketId, Some("MARKET_ID")) else {
        // ts:1699
        return Err("market needs a system name, or --market-id <id> (or MARKET_ID in the environment)".to_owned().into());
    };
    js::parse_unsigned_integer("MARKET_ID", from_environment)
        .map(MarketTarget::Single)
        .map_err(CliError::from)
}

/// `args.positionals.join(" ").trim()`, as `None` when it comes out empty.
fn positional_name(cli: &Cli<'_>) -> Option<String> {
    let joined = cli.args().positionals.join(" ");
    let trimmed = text::js_trim(&joined);
    if trimmed.is_empty() { None } else { Some(trimmed.to_owned()) }
}

/// Which Ardent lookup a name gets, from `resolveLocation`'s second argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupMode {
    Station,
    System,
    Auto,
}

/// `market <name>`'s lookup mode (ts:1606).
///
/// Two-valued, not three: the sweep asks only whether `--station` was given,
/// so a `--system` name resolves as `auto` here while `markets` would resolve
/// the same name as `system`.
pub fn sweep_lookup_mode(cli: &Cli<'_>) -> LookupMode {
    if cli.optional_value(Flag::Station, None).is_some() { LookupMode::Station } else { LookupMode::Auto }
}

/// `SweepSettings` (ts:1417).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SweepSettings {
    pub workers: u32,
    pub timeout_ms: f64,
    /// Total tries per market: one initial attempt plus this many requeues.
    pub requeues: f64,
    pub quiet: bool,
    pub detail: bool,
}

/// The settings block of `runMarketSweep` (ts:1631).
///
/// Read *after* two network calls have already happened, which is why this is
/// its own function rather than part of a `market` config struct \[R50\].
///
/// `--concurrency 0` clamps to one worker, because the `Math.max(1, …)` is
/// outermost. `--timeout` and `--requeue` are unbounded: a requeue count of
/// 10¹⁵ is accepted and will be honoured \[R51\], \[R98\].
///
/// `quiet` is `session.json`, passed in rather than re-read so that this
/// function cannot disagree with the session about it.
pub fn sweep_settings(cli: &Cli<'_>, json: bool) -> Result<SweepSettings, CliError> {
    let concurrency = cli.optional_number(Flag::Concurrency)?.unwrap_or(f64::from(DEFAULT_CONCURRENCY));
    let workers = js::js_max(1.0, js::js_min(f64::from(MAX_CONCURRENCY), concurrency));
    let timeout = cli.optional_decimal(Flag::Timeout)?.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    Ok(SweepSettings {
        // Clamped into 1..=MAX_CONCURRENCY above, so the cast is exact.
        workers: workers as u32,
        timeout_ms: js::js_round(timeout * 1_000.0),
        requeues: cli.optional_number(Flag::Requeue)?.unwrap_or(DEFAULT_REQUEUES),
        quiet: json,
        detail: cli.switch_value(Flag::Detail, false)?,
    })
}

// ---------------------------------------------------------------------------
// markets
// ---------------------------------------------------------------------------

/// What `markets` was pointed at.
#[derive(Clone, Debug, PartialEq)]
pub enum MarketsConfig {
    /// `--address <id64>`, which skips the Ardent lookup entirely.
    Address(f64),
    /// A name to resolve, and how to resolve it.
    Lookup { name: String, mode: LookupMode },
}

/// `runMarkets` (ts:2997).
///
/// Name precedence is `--station ?? --system ?? positional` — the **opposite**
/// of `market`'s \[R52\]. Both spellings are read unconditionally before the
/// choice is made, matching the TypeScript's three `const` declarations, and
/// the lookup mode is decided from the same two reads.
///
/// `--address` wins outright: when it is present no name is resolved, so a
/// bogus `--station` alongside it is never sent anywhere.
pub fn markets_config(cli: &Cli<'_>) -> Result<MarketsConfig, CliError> {
    let explicit_address = cli.optional_number(Flag::Address)?;
    let station_name = cli.optional_value(Flag::Station, None);
    let system_name = cli.optional_value(Flag::System, None);
    let positional = positional_name(cli);
    let name = station_name.or(system_name).map(str::to_owned).or(positional);

    if let Some(address) = explicit_address {
        return Ok(MarketsConfig::Address(address));
    }
    let Some(name) = name else {
        // ts:3011
        return Err("markets needs a system or station name (or --address <id64>)".to_owned().into());
    };
    let mode = if station_name.is_some() {
        LookupMode::Station
    } else if system_name.is_some() {
        LookupMode::System
    } else {
        LookupMode::Auto
    };
    Ok(MarketsConfig::Lookup { name, mode })
}

/// Where a starsystem request's `cachedTimeStamp` comes from \[R51\].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachedTimestamp {
    /// `markets` honours `--cached-timestamp`, defaulting to 0 (ts:3043).
    Flag,
    /// The sweep passes a literal `0` and never reads the flag (ts:1616), so
    /// `market Colonia --cached-timestamp x` is accepted and ignored.
    SweepZero,
}

/// The two envelope fields a `/2.0/elite/starsystem` request takes from the
/// command line.
#[derive(Clone, Debug, PartialEq)]
pub struct StarsystemQuery {
    pub language: String,
    pub cached_timestamp: f64,
}

/// `--language` and `--cached-timestamp`, in that order (ts:3042, ts:1616).
///
/// Both are read *after* the Ardent lookup and, for `markets`, after the SYSTEM
/// table has been printed — so a malformed `--cached-timestamp` surfaces with a
/// table already on stdout \[R50\].
pub fn starsystem_query(
    cli: &Cli<'_>,
    cached: CachedTimestamp,
) -> Result<StarsystemQuery, CliError> {
    let language = cli.optional_value(Flag::Language, None).unwrap_or("en").to_owned();
    let cached_timestamp = match cached {
        CachedTimestamp::Flag => cli.optional_number(Flag::CachedTimestamp)?.unwrap_or(0.0),
        CachedTimestamp::SweepZero => 0.0,
    };
    Ok(StarsystemQuery { language, cached_timestamp })
}

// ---------------------------------------------------------------------------
// EDDN
// ---------------------------------------------------------------------------

/// `EddnOptions` (ts:2854) is already modelled by the domain layer; this is the
/// name the CLI knows it by.
pub type EddnConfig = EddnOptions;

/// Does this run publish to EDDN? (ts:1602)
///
/// `||` short-circuits, so with `--eddn` set the `--eddn-test` slot is never
/// read — and a poisoned `--eddn-test` therefore does not throw \[R47\].
pub fn wants_eddn(cli: &Cli<'_>) -> Result<bool, CliError> {
    if cli.switch_value(Flag::Eddn, false)? {
        return Ok(true);
    }
    cli.switch_value(Flag::EddnTest, false)
}

/// `loadEddnOptions` (ts:2866).
///
/// The uploader falls back to the commander id, which is why this needs the
/// credentials the session already loaded.
pub fn eddn_config(cli: &Cli<'_>, credentials: &Credentials) -> Result<EddnConfig, CliError> {
    let defaults = EddnOptions::default();
    Ok(EddnOptions {
        test: cli.switch_value(Flag::EddnTest, false)?,
        uploader_id: cli
            .optional_value(Flag::Uploader, None)
            .map_or_else(|| credentials.commander_id.clone(), str::to_owned),
        software_name: cli
            .optional_value(Flag::SoftwareName, None)
            .map_or(defaults.software_name, str::to_owned),
        software_version: cli
            .optional_value(Flag::SoftwareVersion, None)
            .map_or(defaults.software_version, str::to_owned),
        game_version: cli
            .optional_value(Flag::GameVersion, None)
            .map_or(defaults.game_version, str::to_owned),
        game_build: cli.optional_value(Flag::GameBuild, None).unwrap_or("").to_owned(),
        horizons: cli.optional_switch(Flag::Horizons)?,
        odyssey: cli.optional_switch(Flag::Odyssey)?,
    })
}

// ---------------------------------------------------------------------------
// trade
// ---------------------------------------------------------------------------

/// `splitItems` (ts:2321).
pub fn split_items(raw: &str) -> Result<Vec<String>, CliError> {
    let items: Vec<String> = raw
        .split(',')
        .map(|token| text::js_trim(token).to_owned())
        .filter(|token| !token.is_empty())
        .collect();
    if items.is_empty() {
        // ts:2323
        return Err("--item needs at least one commodity".to_owned().into());
    }
    Ok(items)
}

/// How `runTrade` (ts:2327) dispatches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeDispatch {
    /// `--item` split on commas and trimmed.
    ///
    /// The single-trade path does **not** use these: `resolveTrade` re-reads
    /// the raw, unsplit flag, so `--item "gold,"` splits to one item, takes the
    /// single path, and then looks up a commodity literally named `gold,`
    /// \[R54\]. Reproduced, and it is a bug in the original.
    pub items: Vec<String>,
    pub batch: bool,
}

/// `runTrade` (ts:2327).
///
/// The `||` chain short-circuits: with two or more items neither `--fill` nor
/// `--watch` is read, so a poisoned switch there goes unnoticed \[R47\].
pub fn trade_dispatch(cli: &Cli<'_>) -> Result<TradeDispatch, CliError> {
    let items = split_items(cli.require_value(Flag::Item, None)?)?;
    let batch = items.len() > 1
        || cli.switch_value(Flag::Fill, false)?
        || cli.switch_value(Flag::Watch, false)?;
    Ok(TradeDispatch { items, batch })
}

/// What `runSingleTrade` (ts:1935) reads before it fetches anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeInputs {
    pub resolve: bool,
    /// The market to read the listing from — `Some` only when resolving,
    /// because the TypeScript's `requireValue` sits inside the `if (resolve)`
    /// branch. With `--no-resolve` a missing `--market-id` is not diagnosed
    /// here but by `resolveTrade`, several reads later.
    pub market_id: Option<String>,
}

/// `runSingleTrade`'s pre-fetch reads (ts:1936).
pub fn trade_inputs(cli: &Cli<'_>) -> Result<TradeInputs, CliError> {
    let resolve = cli.switch_value(Flag::Resolve, true)?;
    let market_id = if resolve {
        Some(cli.require_value(Flag::MarketId, Some("MARKET_ID"))?.to_owned())
    } else {
        None
    };
    Ok(TradeInputs { resolve, market_id })
}

/// Where a resolved value came from, so the plan table can show its provenance
/// (ts:1722).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanSource {
    Flag,
    Market,
    Default,
}

impl PlanSource {
    /// The cell text in the plan table's `From` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Market => "market",
            Self::Default => "default",
        }
    }
}

/// One row of the plan table (ts:1724).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanField {
    pub label: &'static str,
    pub value: String,
    pub source: PlanSource,
}

/// `ResolvedTrade` (ts:1802) without the snapshot, which the caller already
/// holds.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTrade {
    pub plan: TradePlan,
    pub fields: Vec<PlanField>,
    pub notes: Vec<String>,
}

/// `/^\d+$/` — ASCII digits, at least one.
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `resolveTrade` (ts:1810).
///
/// The read order is the specification \[R50\], \[R94\]: market-id, type, item,
/// qty, the zero-qty rejection, unit-price, black-market, stolen, cap, the
/// commodity lookup, the two `--no-resolve` guards, the price, the stock clamp,
/// **then** `--cargo`, the free-space clamp, and finally `--final-qty`.
///
/// Two consequences worth stating because they look like bugs:
///
/// * `--cargo` is parsed *after* the stock clamp has had its chance to throw,
///   so `--cargo abc` on a commodity with no stock reports the empty stock.
/// * `--cargo` is parsed *inside* the `cap && commodity` branch, so
///   `--no-cap --cargo abc` never validates `--cargo` at all.
#[expect(
    clippy::too_many_lines,
    reason = "R50: the function is one ordered sequence of reads and throws; splitting it into \
              helpers would put the order behind a call graph, which is exactly what must stay \
              legible here"
)]
pub fn resolve_trade(
    cli: &Cli<'_>,
    snapshot: Option<&MarketSnapshot<'_>>,
) -> Result<ResolvedTrade, CliError> {
    let market_id = cli.require_value(Flag::MarketId, Some("MARKET_ID"))?.to_owned();
    let raw_type = cli.require_value(Flag::Type, None)?.to_lowercase();
    // ts:1814
    let kind = Kind::parse(&raw_type).map_err(CliError::from)?;

    let item = cli.require_value(Flag::Item, None)?;
    let Some(requested_qty) = cli.optional_number(Flag::Qty)? else {
        // ts:1819
        return Err("Missing required option --qty".to_owned().into());
    };
    if requested_qty == 0.0 {
        // ts:1820
        return Err("--qty must be at least 1".to_owned().into());
    }

    let explicit_price = cli.optional_number(Flag::UnitPrice)?;
    let explicit_black_market = cli.optional_switch(Flag::BlackMarket)?;
    let stolen = cli.switch_value(Flag::Stolen, false)?;
    let cap_qty = cli.switch_value(Flag::Cap, true)?;

    let commodity: Option<&Commodity<'_>> = match snapshot {
        Some(snapshot) => {
            Some(domain::find_commodity(&snapshot.commodities, item).map_err(CliError::from)?)
        }
        None => None,
    };
    if commodity.is_none() && !is_digits(item) {
        // ts:1828
        return Err("--item must be a numeric id when --no-resolve is used".to_owned().into());
    }
    if commodity.is_none() && explicit_price.is_none() {
        // ts:1829
        return Err("--unit-price is required when --no-resolve is used".to_owned().into());
    }

    let mut fields: Vec<PlanField> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let commodity_id = commodity.map_or_else(|| js::to_number(item), |c| c.id);
    let commodity_name =
        commodity.map_or_else(|| format!("id {}", js::js_number(commodity_id)), |c| c.name.to_owned());
    let black_market = trade::derive_black_market(commodity, stolen, explicit_black_market);

    let mut price_source = PlanSource::Flag;
    let unit_price = match (explicit_price, commodity) {
        (Some(price), _) => price,
        (None, Some(commodity)) => {
            price_source = PlanSource::Market;
            trade::derive_price(commodity, kind, black_market).map_err(CliError::from)?
        }
        // ts:1847. Unreachable behind the ts:1829 guard, which already demands
        // a price whenever there is no commodity; transcribed anyway.
        (None, None) => {
            return Err("Could not determine a unit price; pass --unit-price".to_owned().into());
        }
    };

    let mut qty = requested_qty;
    let mut qty_source = PlanSource::Flag;
    if let Some(commodity) = commodity
        && cap_qty
    {
        // `snapshot` is `Some` whenever `commodity` is: the lookup is the only
        // producer of one. The TypeScript writes `snapshot!` here for the same
        // reason.
        let inventory = snapshot.map_or(&[][..], |s| s.inventory);
        let held = domain::held_quantity(inventory, commodity, stolen);
        let available = trade::available(commodity, held, kind);
        let label = trade::availability_label(kind, stolen);
        if available == 0.0 {
            // ts:1858
            return Err(format!(
                "{}: {label} is 0, nothing to {}. Pass --no-cap to send the request anyway.",
                commodity.name,
                kind.as_str(),
            )
            .into());
        }
        if qty > available {
            // ts:1862
            notes.push(format!(
                "--qty {} clamped to {label} {}",
                js::format_integer(requested_qty),
                js::format_integer(available),
            ));
            qty = available;
            qty_source = PlanSource::Market;
        }

        // A buy cannot exceed the room left in the hold, when a capacity is
        // known — and this is where `--cargo` is finally parsed \[R94\].
        let cargo = cli.optional_number(Flag::Cargo)?;
        if let Some(cargo) = cargo
            && kind == Kind::Buy
        {
            let free = Space::of(Some(cargo), domain::cargo_used(inventory));
            if free.exhausted() {
                // ts:1871
                return Err(format!(
                    "Cargo is full ({} units); nothing can be bought",
                    js::format_integer(cargo),
                )
                .into());
            }
            if qty > free.units() {
                // ts:1873
                notes.push(format!(
                    "qty {} clamped to free cargo space {}",
                    js::format_integer(qty),
                    js::format_integer(free.units()),
                ));
                qty = free.units();
                qty_source = PlanSource::Market;
            }
        }
    }

    let explicit_final_qty = cli.optional_number(Flag::FinalQty)?;
    let final_qty = match (explicit_final_qty, commodity) {
        (Some(explicit), _) => explicit,
        (None, Some(commodity)) => {
            let inventory = snapshot.map_or(&[][..], |s| s.inventory);
            trade::resulting_stack(
                domain::held_quantity(inventory, commodity, stolen),
                qty,
                kind,
            )
        }
        (None, None) => {
            // ts:1889
            notes.push(
                "--no-resolve: finalQty falls back to qty, which is only right if you hold none of this commodity"
                    .to_owned(),
            );
            qty
        }
    };

    let mut record = |label: &'static str, value: String, source: PlanSource| {
        fields.push(PlanField { label, value, source });
    };
    record(
        "marketId",
        market_id.clone(),
        // Truthiness of the *string*: `optionalValue` never yields blank, so
        // this is presence — and it deliberately omits the `MARKET_ID`
        // fallback, which is why an environment-supplied id reads "default".
        if cli.optional_value(Flag::MarketId, None).is_some() {
            PlanSource::Flag
        } else {
            PlanSource::Default
        },
    );
    record("transactionType", kind.as_str().to_owned(), PlanSource::Flag);
    record(
        "commodityId",
        format!("{} ({commodity_name})", js::js_number(commodity_id)),
        if is_digits(item) { PlanSource::Flag } else { PlanSource::Market },
    );
    record(
        "blackMarket",
        if black_market { "1" } else { "0" }.to_owned(),
        if explicit_black_market.is_none() { PlanSource::Market } else { PlanSource::Flag },
    );
    record(
        "stolen",
        if stolen { "1" } else { "0" }.to_owned(),
        if cli.optional_switch(Flag::Stolen)?.is_none() {
            PlanSource::Default
        } else {
            PlanSource::Flag
        },
    );
    record("unitPrice", js::format_integer(unit_price), price_source);
    record("qty", js::format_integer(qty), qty_source);
    record(
        "finalQty",
        js::format_integer(final_qty),
        if explicit_final_qty.is_some() {
            PlanSource::Flag
        } else if commodity.is_some() {
            PlanSource::Market
        } else {
            PlanSource::Default
        },
    );
    record("total", format!("{} cr", js::format_integer(unit_price * qty)), PlanSource::Default);

    if let Some(commodity) = commodity {
        let inventory = snapshot.map_or(&[][..], |s| s.inventory);
        let held = domain::held_quantity(inventory, commodity, stolen);
        // ts:1897
        notes.push(format!(
            "{}: stock {} | demand {} | buy {} | sell {} | fence {} | held {}",
            commodity.name,
            js::format_quantity(commodity.stock),
            js::format_quantity(commodity.demand),
            js::format_quantity(commodity.buy_price),
            js::format_quantity(commodity.sell_price),
            js::format_quantity(commodity.fence_price),
            js::format_quantity(held),
        ));
    }

    Ok(ResolvedTrade {
        plan: TradePlan {
            market_id,
            kind,
            commodity_id,
            commodity_name,
            black_market,
            stolen,
            unit_price,
            qty,
            final_qty,
        },
        fields,
        notes,
    })
}

// ---------------------------------------------------------------------------
// batch trade
// ---------------------------------------------------------------------------

/// `BatchSettings` (ts:1995).
#[derive(Clone, Debug, PartialEq)]
pub struct BatchConfig {
    /// Never parsed as a number \[R53\] — it reaches the wire verbatim.
    pub market_id: String,
    pub kind: Kind,
    pub items: Vec<String>,
    pub fill: bool,
    pub cargo: Option<f64>,
    /// Per-commodity ceiling; required unless `--fill` decides the amount.
    pub per_item_qty: Option<f64>,
    pub stolen: bool,
    pub explicit_black_market: Option<bool>,
    pub explicit_price: Option<f64>,
    pub watch: bool,
    pub interval_ms: f64,
    pub attempt_limit: f64,
    /// Starting balance, if known; otherwise learned from the first reply.
    pub credits: Option<f64>,
}

/// `loadBatchSettings` (ts:2051).
///
/// Sixteen steps in a fixed order \[R50\]: seven reads, eight guards, and then
/// the returned object literal — whose first property is `marketId`, so the
/// `MARKET_ID` requirement is diagnosed **after** every guard but **before**
/// `--stolen`, `--black-market`, `--unit-price` and `--credits` are read. That
/// is not a design choice, it is JavaScript evaluating object properties in
/// source order.
pub fn batch_config(cli: &Cli<'_>, items: Vec<String>) -> Result<BatchConfig, CliError> {
    let raw_type = cli.require_value(Flag::Type, None)?.to_lowercase();
    // ts:2054
    let kind = Kind::parse(&raw_type).map_err(CliError::from)?;

    let fill = cli.switch_value(Flag::Fill, false)?;
    let cargo = cli.optional_number(Flag::Cargo)?;
    let per_item_qty = cli.optional_number(Flag::Qty)?;
    let watch = cli.switch_value(Flag::Watch, false)?;
    let attempt_limit = cli.optional_number(Flag::Attempts)?.unwrap_or(0.0);
    let interval = cli.optional_decimal(Flag::Interval)?.unwrap_or(1.0);

    if fill && kind != Kind::Buy {
        // ts:2063
        return Err("--fill only applies to --type buy".to_owned().into());
    }
    if fill && cargo.is_none() {
        // ts:2064
        return Err("--fill needs --cargo <capacity> to know when the hold is full".to_owned().into());
    }
    if fill && !cli.switch_value(Flag::Cap, true)? {
        // ts:2065
        return Err("--fill cannot be combined with --no-cap".to_owned().into());
    }
    if !fill && per_item_qty.is_none() {
        // ts:2066
        return Err("Missing required option --qty (or pass --fill)".to_owned().into());
    }
    // Tested even under `--fill`, which is how `--fill --qty 0` is rejected.
    if per_item_qty == Some(0.0) {
        // ts:2067
        return Err("--qty must be at least 1".to_owned().into());
    }
    // The message names two situations, but the guard is unconditional: every
    // batch run refuses `--no-resolve`, including a plain two-item one.
    if !cli.switch_value(Flag::Resolve, true)? {
        // ts:2068
        return Err("--no-resolve cannot be used with --fill or multiple items".to_owned().into());
    }
    if watch && !fill && attempt_limit == 0.0 {
        // ts:2070
        return Err("--watch needs --fill (or --attempts <n>) so it has a stopping condition".to_owned().into());
    }
    #[expect(
        clippy::manual_range_contains,
        reason = "transcribed from ts:2072; a range would read as one test where the TypeScript \
                  has two, and the two-sided form is what a reviewer diffs against the source"
    )]
    if interval < 0.1 || interval > 3_600.0 {
        // ts:2072
        return Err("--interval must be between 0.1 and 3600 seconds".to_owned().into());
    }

    Ok(BatchConfig {
        market_id: cli.require_value(Flag::MarketId, Some("MARKET_ID"))?.to_owned(),
        kind,
        items,
        fill,
        cargo,
        per_item_qty,
        stolen: cli.switch_value(Flag::Stolen, false)?,
        explicit_black_market: cli.optional_switch(Flag::BlackMarket)?,
        explicit_price: cli.optional_number(Flag::UnitPrice)?,
        watch,
        interval_ms: js::js_round(interval * 1_000.0),
        attempt_limit,
        credits: cli.optional_number(Flag::Credits)?,
    })
}

// ---------------------------------------------------------------------------
// route
//
// A new command, so nothing here is transcribed and nothing is bound by the
// parity register. What it does inherit is the discipline: reads happen in a
// stated order, every default is named, and a filter records whether it prunes
// before the Companion API is touched or only at ranking time. That last
// distinction is the request count, which is the whole cost of the feature.
// ---------------------------------------------------------------------------

/// Which shapes of route to search for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A single laden hop. Has no repeatable rate: flying it again means
    /// deadheading back empty.
    OneWay,
    /// `A -> B -> A`, a different commodity each way.
    RoundTrip,
    /// The best repeatable cycle of any length. Exactly solvable.
    Loop,
    /// The best repeatable cycle of at most this many stops.
    BoundedLoop(usize),
}

impl Shape {
    /// `--shape one-way|round-trip|loop|loop:N`.
    pub fn parse(raw: &str) -> Result<Self, CliError> {
        let raw = crate::js::text::js_trim(raw);
        if let Some(bound) = raw.strip_prefix("loop:") {
            let stops = crate::js::parse_unsigned_integer("--shape loop:N", bound)
                .map_err(CliError::from)?;
            if stops < 2.0 {
                return Err("--shape loop:N needs at least 2 stops".to_owned().into());
            }
            return Ok(Self::BoundedLoop(stops as usize));
        }
        match raw {
            "one-way" | "oneway" => Ok(Self::OneWay),
            "round-trip" | "roundtrip" => Ok(Self::RoundTrip),
            "loop" => Ok(Self::Loop),
            other => Err(format!(
                "--shape must be one-way, round-trip, loop or loop:N, not \"{other}\""
            )
            .into()),
        }
    }
}

/// The largest ship that can berth. Advisory only against Ardent's own field,
/// which is measurably unreliable; the station-type filter is what actually
/// decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pad {
    Small,
    Medium,
    Large,
}

impl Pad {
    pub fn parse(raw: &str) -> Result<Self, CliError> {
        match crate::js::text::js_trim(raw).to_lowercase().as_str() {
            "s" | "small" | "1" => Ok(Self::Small),
            "m" | "medium" | "2" => Ok(Self::Medium),
            "l" | "large" | "3" => Ok(Self::Large),
            other => Err(format!("--pad must be S, M or L, not \"{other}\"").into()),
        }
    }
}

/// Everything `route` was asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteConfig {
    /// The system or station to search around.
    pub reference: String,
    pub radius_ly: f64,

    // Pruned before the Companion API is touched.
    pub pad: Pad,
    pub station_types: Option<Vec<String>>,
    pub include_carriers: bool,
    pub include_settlements: bool,
    pub max_star_distance_ls: Option<f64>,

    // Applied at ranking time only.
    pub cargo: Option<f64>,
    pub credits: Option<f64>,
    pub jump_range_ly: f64,
    pub shape: Shape,
    pub top: usize,
    pub min_profit: f64,
    pub min_supply: f64,
    pub min_demand: f64,
    /// `--include-illegal`. See [`edm_route`'s `RowFloors::allow_illegal`]: a
    /// market that calls a commodity illegal refuses the trade at the counter.
    pub include_illegal: bool,
    /// `--category`: only these `categoryname`s are ranked, lowercased. Empty
    /// means every category.
    pub categories: Vec<String>,
    /// `--eddn` / `--eddn-test`: relay every market this run polls live.
    pub eddn: bool,
    /// `--eddn-max-age`, in minutes: how long a market stays suppressed after
    /// this machine relayed it.
    pub eddn_max_age_minutes: f64,

    // Spending.
    /// Workers behind the pacer.
    ///
    /// `--concurrency`, the same flag and the same clamp the ported sweep
    /// uses, because it means the same thing. It decides only how much
    /// per-request latency is hidden: the rate limit is what bounds the run,
    /// and eight workers behind a four-per-second bucket still issue four per
    /// second.
    pub workers: u32,
    pub rate_per_second: f64,
    /// `--deadline`, in seconds: how long the whole run may take.
    ///
    /// It bounds one job, because a market may not keep being retried for
    /// longer than the run it is part of has left. That is the rule that
    /// replaces R98's unbounded attempt count: wall clock, not attempts.
    ///
    /// It bounds the **loop search** for the same reason, and this is the one
    /// wall clock there is: the search is part of the run, and a second flag
    /// would let a user set two limits that contradict each other. The
    /// optimiser is pure and has no clock of its own, so `cmd::route` reads
    /// this one on its behalf — see `edm_route::watch`.
    pub deadline_seconds: f64,
    pub max_requests: f64,
    pub confirmed: bool,
    pub max_age_minutes: f64,
    pub cache: bool,
    pub refresh: bool,
    pub cache_dir: Option<String>,
    pub ardent_queries: u32,
    pub fast_estimate: bool,
    /// `--verify-systems`. See [`Flag::VerifySystems`] — off by default, and
    /// the reason the default plan prices no starsystem reads at all.
    pub verify_systems: bool,

    pub dry_run: bool,
    pub json: bool,
    pub detail: bool,
    /// `--verbose`: pacing decisions, retries and cache outcomes as they happen.
    ///
    /// Separate from the per-market progress lines, which are on by default. A
    /// sweep that is merely slow and a sweep that is being throttled look
    /// identical from the outside, and this is what tells them apart.
    pub verbose: bool,
    /// Suppress the per-market progress lines.
    ///
    /// Set by `--json`, where they would corrupt the document, and by
    /// `--quiet`. Not a flag of its own beyond that: a sweep that prints
    /// nothing for five minutes is the default nobody wants.
    pub quiet: bool,
}

/// Defaults, in one place so the plan table and the documentation cannot drift
/// apart from the behaviour.
pub const DEFAULT_RADIUS_LY: f64 = 30.0;
pub const DEFAULT_JUMP_RANGE_LY: f64 = 30.0;
pub const DEFAULT_MAX_STAR_DISTANCE_LS: f64 = 2_000.0;
pub const DEFAULT_TOP: f64 = 20.0;
pub const DEFAULT_MIN_PROFIT: f64 = 1_000.0;
pub const DEFAULT_MAX_AGE_MINUTES: f64 = 30.0;
pub const DEFAULT_RPS: f64 = 4.0;
/// An hour. Long enough for a thousand markets at four a second with room for
/// retries, and short enough that a run nobody is watching ends.
/// How long a market stays suppressed after this machine relayed it.
///
/// EDDN is a shared firehose that other people's tools consume, and a second
/// copy of an unchanged listing carries a *newer* timestamp — so it looks like
/// fresh confirmation of a price nobody re-read. Half an hour is the same order
/// as the price cache's own default, which is what makes the two agree: a run
/// inside that window mostly reads from cache and so has nothing to relay
/// anyway.
pub const DEFAULT_EDDN_MAX_AGE_MINUTES: f64 = 30.0;
pub const DEFAULT_DEADLINE_SECONDS: f64 = 3_600.0;
pub const DEFAULT_ARDENT_QUERIES: f64 = 200.0;

/// Builds the configuration for `route`.
///
/// The reference is read first and is required, because every other flag is
/// meaningless without somewhere to search around — and because a run that is
/// going to fail should fail before it has printed anything.
pub fn route_config(cli: &Cli<'_>) -> Result<RouteConfig, CliError> {
    // A positional beginning with `-` is a mistyped flag, not part of a name.
    //
    // The ported grammar recognises only `--name` and `-h`, so any other
    // single-dash token becomes a positional \[R44\] — and `route` joins its
    // positionals into the reference, because a system name has spaces in it.
    // The two together turned `route Sol -v` into a search for a system called
    // "Sol -v" and reported that Ardent had never heard of it. No Elite system
    // name begins with a hyphen, so refusing them costs nothing and the message
    // names the token rather than blaming the name.
    if let Some(stray) = cli.args().positionals.iter().find(|token| token.starts_with('-')) {
        return Err(format!("Unknown option {stray}").into());
    }
    let positional = cli.args().positionals.join(" ");
    let positional = crate::js::text::js_trim(&positional);
    let reference = match cli.optional_value(Flag::System, None) {
        Some(system) => system.to_owned(),
        None if !positional.is_empty() => positional.to_owned(),
        None => {
            return Err("route needs a system or station name to search around"
                .to_owned()
                .into());
        }
    };

    let radius_ly = cli.optional_decimal(Flag::Radius)?.unwrap_or(DEFAULT_RADIUS_LY);
    let shape = match cli.optional_value(Flag::Shape, None) {
        Some(raw) => Shape::parse(raw)?,
        None => Shape::RoundTrip,
    };
    let pad = match cli.optional_value(Flag::Pad, None) {
        Some(raw) => Pad::parse(raw)?,
        None => Pad::Large,
    };

    Ok(RouteConfig {
        reference,
        radius_ly,
        pad,
        station_types: cli.optional_value(Flag::StationTypes, None).map(|raw| {
            raw.split(',')
                .map(|kind| crate::js::text::js_trim(kind).to_owned())
                .filter(|kind| !kind.is_empty())
                .collect()
        }),
        // Carriers jump without warning and price idiosyncratically, so a route
        // through one can evaporate between planning and flying. They are also
        // over a third of the priced markets in range, so excluding them is
        // most of the request budget too.
        include_carriers: cli.switch_value(Flag::Carriers, false)?,
        // Odyssey settlements are 63% of what Ardent calls a station near Sol
        // and cannot berth a large ship at all, so excluding them is not a cost
        // saving, it is correctness.
        include_settlements: cli.switch_value(Flag::Settlements, false)?,
        max_star_distance_ls: Some(
            cli.optional_decimal(Flag::MaxStarDistance)?.unwrap_or(DEFAULT_MAX_STAR_DISTANCE_LS),
        ),
        cargo: cli.optional_number(Flag::Cargo)?,
        credits: cli.optional_number(Flag::Credits)?,
        jump_range_ly: cli.optional_decimal(Flag::Jump)?.unwrap_or(DEFAULT_JUMP_RANGE_LY),
        shape,
        top: cli.optional_number(Flag::Top)?.unwrap_or(DEFAULT_TOP) as usize,
        min_profit: cli.optional_number(Flag::MinProfit)?.unwrap_or(DEFAULT_MIN_PROFIT),
        min_supply: cli.optional_number(Flag::MinSupply)?.unwrap_or(1.0),
        min_demand: cli.optional_number(Flag::MinDemand)?.unwrap_or(1.0),
        workers: {
            let declared =
                cli.optional_number(Flag::Concurrency)?.unwrap_or(f64::from(DEFAULT_CONCURRENCY));
            // Clamped into 1..=MAX_CONCURRENCY, so the cast is exact.
            js::js_max(1.0, js::js_min(f64::from(MAX_CONCURRENCY), declared)) as u32
        },
        rate_per_second: cli.optional_decimal(Flag::Rps)?.unwrap_or(DEFAULT_RPS),
        deadline_seconds: cli.optional_decimal(Flag::Deadline)?.unwrap_or(DEFAULT_DEADLINE_SECONDS),
        max_requests: cli
            .optional_number(Flag::MaxRequests)?
            .unwrap_or(crate::spend::DEFAULT_MAX_REQUESTS),
        confirmed: cli.switch_value(Flag::Yes, false)?,
        max_age_minutes: cli.optional_decimal(Flag::MaxAge)?.unwrap_or(DEFAULT_MAX_AGE_MINUTES),
        cache: cli.switch_value(Flag::Cache, true)?,
        refresh: cli.switch_value(Flag::Refresh, false)?,
        cache_dir: cli.optional_value(Flag::CacheDir, None).map(str::to_owned),
        ardent_queries: cli
            .optional_number(Flag::ArdentQueries)?
            .unwrap_or(DEFAULT_ARDENT_QUERIES) as u32,
        fast_estimate: cli.switch_value(Flag::FastEstimate, false)?,
        verify_systems: cli.switch_value(Flag::VerifySystems, false)?,
        dry_run: cli.switch_value(Flag::DryRun, false)?,
        json: cli.switch_value(Flag::Json, false)?,
        detail: cli.switch_value(Flag::Detail, false)?,
        verbose: cli.switch_value(Flag::Verbose, false)?,
        include_illegal: cli.switch_value(Flag::IncludeIllegal, false)?,
        categories: cli
            .optional_value(Flag::Category, None)
            .map(split_items)
            .transpose()?
            .unwrap_or_default(),
        eddn: wants_eddn(cli)?,
        eddn_max_age_minutes: cli
            .optional_decimal(Flag::EddnMaxAge)?
            .unwrap_or(DEFAULT_EDDN_MAX_AGE_MINUTES),
        quiet: cli.switch_value(Flag::Json, false)?,
    })
}
