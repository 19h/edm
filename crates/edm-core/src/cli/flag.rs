//! Flag identity: normalisation, aliasing, arity and the spelling used in
//! messages.
//!
//! The TypeScript keeps four separate tables keyed by strings — `FLAG_ALIASES`,
//! `VALUE_FLAGS`, `BOOLEAN_FLAGS` and `FLAG_DISPLAY` — and a fifth,
//! `BOOLEAN_LITERALS`, keyed by the *token* rather than the flag. Collapsing
//! the first four into one enum makes two facts checkable at compile time that
//! were only conventions in the original: that the value set and the switch set
//! are disjoint, and that every canonical name has a documented spelling.

/// A flag's canonical identity, after aliases are folded in.
///
/// The declaration order is load-bearing. Every value-taking flag precedes
/// every switch, so [`Flag::takes_value`] is a single comparison and the
/// slot-type invariant (`Value::Text` iff `takes_value()`) can be stated as a
/// property of the discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Flag {
    // `VALUE_FLAGS` (`market-request.ts:832-869`), in source order.
    MarketId,
    CmdrId,
    MachineId,
    MachineToken,
    AuthToken,
    Nonce,
    FTime,
    RequestTime,
    FdevSemver,
    FdevSeason,
    UserAgent,
    Method,
    Type,
    Item,
    Qty,
    FinalQty,
    UnitPrice,
    Cargo,
    Interval,
    Attempts,
    Credits,
    System,
    Station,
    Address,
    Language,
    CachedTimestamp,
    Dump,
    Uploader,
    GameVersion,
    GameBuild,
    SoftwareName,
    SoftwareVersion,
    StationType,
    Concurrency,
    Timeout,
    Requeue,

    // Route-only value flags. They sit inside the value run rather than after
    // the switches because `takes_value` is a single comparison against the
    // arity boundary; putting them at the end would make it two.
    //
    // None of these resolve unless the command is `route` \[C26\] — see
    // [`Flag::resolve_in`]. Their discriminants shift the switches along, which
    // is harmless: the slot index is never persisted or compared across builds.
    Radius,
    Pad,
    StationTypes,
    MaxStarDistance,
    Jump,
    Shape,
    Top,
    MinProfit,
    MinSupply,
    MinDemand,
    MaxAge,
    Rps,
    MaxRequests,
    RetryBudget,
    Deadline,
    ArdentQueries,
    CacheDir,

    // `BOOLEAN_FLAGS` (`market-request.ts:871-891`), in source order. The first
    // of these is the arity boundary; keep `DryRun` first.
    DryRun,
    FullUrl,
    Json,
    BlackMarket,
    Stolen,
    Resolve,
    Cap,
    FullMarket,
    Fill,
    Watch,
    Carriers,
    Trading,
    Eddn,
    EddnTest,
    Horizons,
    Odyssey,
    Detail,
    AllMarkets,
    Help,

    // Route-only switches, after every base flag so the base grammar's own
    // ordering is untouched.
    Yes,
    Settlements,
    /// `--cache` / `--no-cache`. Named for the positive so the parser's own
    /// `--no-` negation supplies the other spelling; a flag literally called
    /// `no-cache` would be read as a negation of `cache` and rejected.
    Cache,
    Refresh,
    FastEstimate,
    /// `--verbose`: say what the sweep is doing while it does it.
    Verbose,
    /// `--include-illegal`: rank commodities a market marks illegal there.
    IncludeIllegal,
    /// `--verify-systems`: read each system's Companion API `starsystem`
    /// payload rather than trusting Ardent's market list.
    ///
    /// Off by default because it costs about twenty-five times what the market
    /// reads it discovers do — a starsystem payload is ~500 KB against a
    /// market's ~20 KB, and near Sol there is roughly one starport per system.
    /// What it buys is a market Ardent has never seen, which is real but rare.
    VerifySystems,
}

/// Which flag table a parse resolves names against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Table {
    /// Exactly the TypeScript's grammar. Every existing command uses this.
    #[default]
    Base,
    /// `Base` plus the route-only names. Reached only when the command is
    /// `route`.
    Extended,
}

impl Flag {
    /// Every flag, in discriminant order.
    pub const ALL: [Self; 80] = [
        Self::MarketId,
        Self::CmdrId,
        Self::MachineId,
        Self::MachineToken,
        Self::AuthToken,
        Self::Nonce,
        Self::FTime,
        Self::RequestTime,
        Self::FdevSemver,
        Self::FdevSeason,
        Self::UserAgent,
        Self::Method,
        Self::Type,
        Self::Item,
        Self::Qty,
        Self::FinalQty,
        Self::UnitPrice,
        Self::Cargo,
        Self::Interval,
        Self::Attempts,
        Self::Credits,
        Self::System,
        Self::Station,
        Self::Address,
        Self::Language,
        Self::CachedTimestamp,
        Self::Dump,
        Self::Uploader,
        Self::GameVersion,
        Self::GameBuild,
        Self::SoftwareName,
        Self::SoftwareVersion,
        Self::StationType,
        Self::Concurrency,
        Self::Timeout,
        Self::Requeue,
        Self::Radius,
        Self::Pad,
        Self::StationTypes,
        Self::MaxStarDistance,
        Self::Jump,
        Self::Shape,
        Self::Top,
        Self::MinProfit,
        Self::MinSupply,
        Self::MinDemand,
        Self::MaxAge,
        Self::Rps,
        Self::MaxRequests,
        Self::RetryBudget,
        Self::Deadline,
        Self::ArdentQueries,
        Self::CacheDir,
        Self::DryRun,
        Self::FullUrl,
        Self::Json,
        Self::BlackMarket,
        Self::Stolen,
        Self::Resolve,
        Self::Cap,
        Self::FullMarket,
        Self::Fill,
        Self::Watch,
        Self::Carriers,
        Self::Trading,
        Self::Eddn,
        Self::EddnTest,
        Self::Horizons,
        Self::Odyssey,
        Self::Detail,
        Self::AllMarkets,
        Self::Help,
        Self::Yes,
        Self::Settlements,
        Self::Cache,
        Self::Refresh,
        Self::FastEstimate,
        Self::Verbose,
        Self::IncludeIllegal,
        Self::VerifySystems,
    ];

    /// How many distinct flags exist; the width of an [`crate::cli::Args`] slot
    /// table.
    pub const COUNT: usize = Self::ALL.len();

    /// This flag's position in the slot table.
    #[must_use]
    pub fn index(self) -> usize {
        self as usize
    }

    /// Does this flag consume a value (`VALUE_FLAGS`), or is it a switch
    /// (`BOOLEAN_FLAGS`)?
    ///
    /// The two TypeScript sets are disjoint and exhaustive over the canonical
    /// names, which is why this is total rather than three-valued.
    #[must_use]
    pub fn takes_value(self) -> bool {
        (self as u8) < (Self::DryRun as u8)
    }

    /// Which grammar a name is resolved against.
    ///
    /// The base table is the TypeScript's, exactly. Widening it globally would
    /// make `edm market Colonia --pad L` succeed where the original exits 2 —
    /// a fidelity regression on argv the parity harness never runs, which is
    /// the worst kind. So route-only names resolve only for `route` \[C26\].
    #[must_use]
    pub fn resolve_in(normalized: &str, table: Table) -> Option<Self> {
        Self::resolve(normalized).or(match table {
            Table::Base => None,
            Table::Extended => Self::resolve_route(normalized),
        })
    }

    /// The route-only names. Disjoint from the base table by construction, and
    /// a gate proves it.
    #[must_use]
    pub fn resolve_route(normalized: &str) -> Option<Self> {
        Some(match normalized {
            "radius" | "range" => Self::Radius,
            "pad" | "padsize" => Self::Pad,
            "stationtypes" => Self::StationTypes,
            "maxstardistance" | "maxstationdistance" => Self::MaxStarDistance,
            "jump" | "jumprange" => Self::Jump,
            "shape" => Self::Shape,
            "top" => Self::Top,
            "minprofit" => Self::MinProfit,
            "minsupply" => Self::MinSupply,
            "mindemand" => Self::MinDemand,
            "maxage" => Self::MaxAge,
            // Not `--rate`: that is already an alias for `--concurrency`.
            "rps" | "requestspersecond" => Self::Rps,
            "maxrequests" => Self::MaxRequests,
            "retrybudget" => Self::RetryBudget,
            "deadline" => Self::Deadline,
            "ardentqueries" => Self::ArdentQueries,
            "cachedir" => Self::CacheDir,
            "yes" => Self::Yes,
            "settlements" | "includesettlements" => Self::Settlements,
            "cache" => Self::Cache,
            "refresh" => Self::Refresh,
            "fastestimate" => Self::FastEstimate,
            "verbose" | "v" => Self::Verbose,
            "includeillegal" => Self::IncludeIllegal,
            "verifysystems" => Self::VerifySystems,
            // `--carriers` already exists and means exactly what route wants.
            "includecarriers" => Self::Carriers,
            _ => return None,
        })
    }

    /// The spelling a *message* uses: `flagName` (`market-request.ts:976`),
    /// i.e. `--` followed by `FLAG_DISPLAY[flag] ?? flag`.
    ///
    /// This is not what the user typed. Accessor errors report the documented
    /// spelling even when the user reached the flag through an alias, so
    /// `--capacity abc` complains about `--cargo` \[R46\].
    #[must_use]
    pub fn display(self) -> &'static str {
        match self {
            // `FLAG_DISPLAY` (`market-request.ts:961-974`).
            Self::MarketId => "--market-id",
            Self::CmdrId => "--cmdr-id",
            Self::MachineId => "--machine-id",
            Self::MachineToken => "--machine-token",
            Self::AuthToken => "--auth-token",
            Self::FTime => "--f-time",
            Self::RequestTime => "--request-time",
            Self::FdevSemver => "--fdev-semver",
            Self::FdevSeason => "--fdev-season",
            Self::UserAgent => "--user-agent",
            Self::UnitPrice => "--unit-price",
            Self::FinalQty => "--final-qty",
            Self::DryRun => "--dry-run",
            Self::CachedTimestamp => "--cached-timestamp",
            Self::EddnTest => "--eddn-test",
            Self::AllMarkets => "--all-markets",
            Self::StationType => "--station-type",
            Self::GameVersion => "--game-version",
            Self::GameBuild => "--game-build",
            Self::SoftwareName => "--software-name",
            Self::SoftwareVersion => "--software-version",
            Self::FullUrl => "--full-url",
            Self::BlackMarket => "--black-market",
            Self::FullMarket => "--full-market",

            // Route-only. Not in `FLAG_DISPLAY` because the TypeScript has no
            // such command; these are the documented spellings \[C26\].
            Self::Radius => "--radius",
            Self::Pad => "--pad",
            Self::StationTypes => "--station-types",
            Self::MaxStarDistance => "--max-star-distance",
            Self::Jump => "--jump",
            Self::Shape => "--shape",
            Self::Top => "--top",
            Self::MinProfit => "--min-profit",
            Self::MinSupply => "--min-supply",
            Self::MinDemand => "--min-demand",
            Self::MaxAge => "--max-age",
            Self::Rps => "--rps",
            Self::MaxRequests => "--max-requests",
            Self::RetryBudget => "--retry-budget",
            Self::Deadline => "--deadline",
            Self::ArdentQueries => "--ardent-queries",
            Self::CacheDir => "--cache-dir",
            Self::Yes => "--yes",
            Self::Settlements => "--settlements",
            Self::Cache => "--cache",
            Self::Refresh => "--refresh",
            Self::FastEstimate => "--fast-estimate",
            Self::Verbose => "--verbose",
            Self::IncludeIllegal => "--include-illegal",
            Self::VerifySystems => "--verify-systems",

            // Not in `FLAG_DISPLAY`: the `?? flag` fallback prints the
            // canonical key, which for these is already the documented
            // spelling.
            Self::Nonce => "--nonce",
            Self::Method => "--method",
            Self::Type => "--type",
            Self::Item => "--item",
            Self::Qty => "--qty",
            Self::Cargo => "--cargo",
            Self::Interval => "--interval",
            Self::Attempts => "--attempts",
            Self::Credits => "--credits",
            Self::System => "--system",
            Self::Station => "--station",
            Self::Address => "--address",
            Self::Language => "--language",
            Self::Dump => "--dump",
            Self::Uploader => "--uploader",
            Self::Concurrency => "--concurrency",
            Self::Timeout => "--timeout",
            Self::Requeue => "--requeue",
            Self::Json => "--json",
            Self::Stolen => "--stolen",
            Self::Resolve => "--resolve",
            Self::Cap => "--cap",
            Self::Fill => "--fill",
            Self::Watch => "--watch",
            Self::Carriers => "--carriers",
            Self::Trading => "--trading",
            Self::Eddn => "--eddn",
            Self::Horizons => "--horizons",
            Self::Odyssey => "--odyssey",
            Self::Detail => "--detail",
            Self::Help => "--help",
        }
    }

    /// Resolves an already-[`normalize`]d name, folding `FLAG_ALIASES`
    /// (`market-request.ts:804-829`) into the canonical identity.
    ///
    /// `None` is the union of the TypeScript's two rejection paths — a name in
    /// neither set — because the alias lookup can only ever widen the input to
    /// another *string* key, never to a member of one set from the other.
    #[must_use]
    pub fn resolve(normalized: &str) -> Option<Self> {
        Some(match normalized {
            // Canonical value names.
            "marketid" | "market" => Self::MarketId,
            "cmdrid" | "commanderid" | "cmdr" => Self::CmdrId,
            "machineid" => Self::MachineId,
            "machinetoken" => Self::MachineToken,
            "authtoken" => Self::AuthToken,
            "nonce" => Self::Nonce,
            "ftime" => Self::FTime,
            "requesttime" => Self::RequestTime,
            "fdevsemver" => Self::FdevSemver,
            "fdevseason" => Self::FdevSeason,
            "useragent" => Self::UserAgent,
            "method" => Self::Method,
            "type" | "transactiontype" => Self::Type,
            "item" | "commodity" | "commodityid" | "items" | "commodities" => Self::Item,
            "qty" | "quantity" => Self::Qty,
            "finalqty" => Self::FinalQty,
            "unitprice" | "price" => Self::UnitPrice,
            "cargo" | "capacity" | "hold" => Self::Cargo,
            "interval" | "every" => Self::Interval,
            "attempts" | "rounds" => Self::Attempts,
            "credits" => Self::Credits,
            "system" => Self::System,
            "station" => Self::Station,
            "address" | "systemaddr" | "systemaddress" | "id64" => Self::Address,
            "language" | "lang" => Self::Language,
            "cachedtimestamp" => Self::CachedTimestamp,
            "dump" => Self::Dump,
            "uploader" => Self::Uploader,
            "gameversion" => Self::GameVersion,
            "gamebuild" => Self::GameBuild,
            "softwarename" => Self::SoftwareName,
            "softwareversion" => Self::SoftwareVersion,
            "stationtype" => Self::StationType,
            "concurrency" | "rate" | "workers" | "jobs" | "parallel" => Self::Concurrency,
            "timeout" => Self::Timeout,
            "requeue" => Self::Requeue,

            // Canonical switch names.
            "dryrun" => Self::DryRun,
            "fullurl" => Self::FullUrl,
            "json" => Self::Json,
            "blackmarket" => Self::BlackMarket,
            "stolen" => Self::Stolen,
            "resolve" => Self::Resolve,
            "cap" => Self::Cap,
            "fullmarket" => Self::FullMarket,
            "fill" => Self::Fill,
            "watch" | "retry" | "loop" => Self::Watch,
            "carriers" => Self::Carriers,
            "trading" => Self::Trading,
            "eddn" => Self::Eddn,
            "eddntest" => Self::EddnTest,
            "horizons" => Self::Horizons,
            "odyssey" => Self::Odyssey,
            "detail" => Self::Detail,
            "allmarkets" => Self::AllMarkets,
            "help" => Self::Help,

            _ => return None,
        })
    }
}

/// `normalizeFlag` (`market-request.ts:800`) — strip **every** `-` and `_`,
/// then lowercase.
///
/// The lowercasing is full Unicode, not ASCII: `String.prototype.toLowerCase`
/// applies the Unicode default case conversion, so `--mar\u{212A}etid` (with a
/// KELVIN SIGN) normalises to `marketid` and resolves. `to_ascii_lowercase`
/// would leave the KELVIN SIGN standing and report an unknown option \[R41\].
///
/// The two steps happen in this order because that is the order the TypeScript
/// composes them in; no case mapping produces `-` or `_`, so the result would
/// be the same either way.
#[must_use]
pub fn normalize(name: &str) -> String {
    let stripped: String = name.chars().filter(|&c| c != '-' && c != '_').collect();
    stripped.to_lowercase()
}

/// What a token means when a switch is looking for an explicit boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Literal {
    /// One of the eight documented spellings.
    Bool(bool),
    /// A key that reaches `Object.prototype` instead \[R47\]. See
    /// [`Value::Poison`](crate::cli::Value).
    Poison,
}

/// `BOOLEAN_LITERALS[token.toLowerCase()]` (`market-request.ts:893`).
///
/// The lookup is a plain object index, so it does not stop at the object's own
/// properties. Two lowercase spellings — `constructor` and `__proto__` —
/// resolve through `Object.prototype` to a function and to the prototype
/// itself. Neither is `undefined`, so the TypeScript treats them as a
/// successful match: it consumes the token and stores a non-boolean in the flag
/// map, which detonates later in `optionalSwitch`. That is [`Literal::Poison`]
/// \[R47\].
///
/// The other ten `Object.prototype` keys are unreachable here because they all
/// contain capitals or underscores that survive `toLowerCase` (`hasOwnProperty`
/// → `hasownproperty`), and unlike flag names these tokens are **not**
/// separator-stripped.
#[must_use]
pub fn boolean_literal(token: &str) -> Option<Literal> {
    match token.to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(Literal::Bool(true)),
        "0" | "false" | "no" | "off" => Some(Literal::Bool(false)),
        "constructor" | "__proto__" => Some(Literal::Poison),
        _ => None,
    }
}
