//! The program's fixed values, in one place because several of them are
//! interpolated into the usage text and must not drift from the behaviour they
//! describe.

/// Where the game-internal API lives.
pub const API_ORIGIN: &str = "https://api.orerve.net";

/// One game-internal API route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub path: &'static str,
    pub method: &'static str,
}

/// The verbs come from the game's own `methodCode` values: 1 = GET, 3 = PUT.
/// `trade` answers `Allow: PUT, OPTIONS`, and a GET there is rejected with 405.
pub const MARKET_LIST: Endpoint = Endpoint { path: "/2.0/elite/market/list", method: "GET" };
pub const MARKET_TRADE: Endpoint = Endpoint { path: "/2.0/elite/market/trade", method: "PUT" };
pub const STARSYSTEM: Endpoint = Endpoint { path: "/2.0/elite/starsystem", method: "GET" };
/// Read-only bulk market metadata and candidate prices, keyed by system address.
pub const STARSYSTEM_MARKETDATA: Endpoint =
    Endpoint { path: "/2.0/elite/starsystem/marketdata", method: "GET" };
/// Read-only paged snapshot of populated-system topology and status.
pub const STARSYSTEM_DAILY_DIGEST: Endpoint =
    Endpoint { path: "/2.0/elite/starsystem/dailydigest_part", method: "GET" };
/// Read-only server-side market discovery limits and cache policy.
pub const RESOURCE_FINANCE: Endpoint =
    Endpoint { path: "/2.0/elite/resources/finance", method: "GET" };
/// Read-only canonical commodity catalogue.
pub const RESOURCE_COMMODITIES: Endpoint =
    Endpoint { path: "/2.0/elite/resources/commodities", method: "GET" };

/// Frontier's own client batches five system addresses per marketdata request.
pub const MARKETDATA_BATCH_MAX: usize = 5;
pub const MARKETDATA_CACHE_SECONDS_FALLBACK: u64 = 7_200;
pub const MARKETDATA_DISTANCE_LY_FALLBACK: f64 = 40.0;

/// `docs/Developers.md:117` — note the non-standard port and the required
/// trailing slash. Plain HTTP earns a 400.
pub const EDDN_UPLOAD_URL: &str = "https://eddn.edcd.io:4430/upload/";
pub const EDDN_SCHEMA: &str = "https://eddn.edcd.io/schemas/commodity/3";
/// What this program calls itself to EDDN \[C34\].
///
/// The original says `int-market-sync`, and the port said it too — which meant
/// every row this program contributed to a shared dataset was attributed to a
/// different piece of software. EDDN's whole purpose is knowing what produced a
/// datum: when a sender misbehaves, the software name is how consumers and
/// maintainers find it, and when a sender is fixed it is how they know. Being
/// byte-identical to the original is worth a great deal in this port; it is not
/// worth signing somebody else's name.
///
/// `--software-name` still overrides it, as it always did.
pub const EDDN_SOFTWARE_NAME: &str = "edm";
/// MUST be incremented whenever the content of the messages we send changes.
pub const EDDN_SOFTWARE_VERSION: &str = "1.0.0";
/// `docs/Developers.md:263` — commodity data taken from a live game-internal API endpoint.
pub const EDDN_GAME_VERSION: &str = "GameInternal-Live-market";

/// Sweeps run as a pool of workers pulling from one queue, not at a fixed rate.
pub const DEFAULT_CONCURRENCY: u32 = 5;
pub const MAX_CONCURRENCY: u32 = 16;
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 10.0;
pub const DEFAULT_REQUEUES: f64 = 3.0;

/// Default request headers, overridable by flag or environment.
pub const DEFAULT_FDEV_SEMVER: &str = "4.4.0.3";
pub const DEFAULT_FDEV_SEASON: &str = "4";
pub const DEFAULT_USER_AGENT: &str = "EDGame/11.0/Win64";

/// Ardent is the only route that maps a market id back to its names.
pub const ARDENT_BASE_URL: &str = "https://api.ardent-insight.com/v2";

/// The credential field widths the game-internal API expects.
pub const MACHINE_TOKEN_LENGTH: usize = 80;
/// The shortest `authToken` that is plausibly a token rather than a truncated
/// paste.
///
/// **The original demands exactly 2024 \[C31\].** A live token measured
/// 2026-08-06 is 2022 characters, so that constant is not a property of
/// Frontier's tokens — it is one observation written down as a law, and it
/// makes the program refuse a credential the game itself is using. The check is
/// worth keeping because a half-pasted token is a real mistake with a confusing
/// failure; the exact length is not, because it cannot be right for a value
/// this program does not issue.
pub const AUTH_TOKEN_MIN_LENGTH: usize = 512;
