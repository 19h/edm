//! The `--help` text, and the constants it advertises.
//!
//! `USAGE` is a JavaScript template literal (`game-internal-api.ts:1029`), not a
//! frozen block of prose: fourteen of its values are interpolated from the same
//! constants the program actually runs on. Freezing the rendered text here
//! would let a default drift away from its own documentation silently, so the
//! interpolation is reproduced and `usage_matches_its_interpolation` holds the
//! two together.

use crate::js;

/// `MARKET_LIST.method` (`game-internal-api.ts:18`).
///
/// The verbs are the game's own `methodCode` values: 1 = GET, 3 = PUT.
pub const MARKET_LIST_METHOD: &str = "GET";
/// `MARKET_LIST.path` / `MARKET_LIST_PATH` (`game-internal-api.ts:18`).
pub const MARKET_LIST_PATH: &str = "/2.0/elite/market/list";
/// `MARKET_TRADE.method` (`game-internal-api.ts:19`) — trade answers
/// `Allow: PUT, OPTIONS`, so a GET there is a 405.
pub const MARKET_TRADE_METHOD: &str = "PUT";
/// `MARKET_TRADE.path` / `MARKET_TRADE_PATH` (`game-internal-api.ts:19`).
pub const MARKET_TRADE_PATH: &str = "/2.0/elite/market/trade";
/// `STARSYSTEM.method` (`game-internal-api.ts:20`).
pub const STARSYSTEM_METHOD: &str = "GET";
/// `STARSYSTEM.path` (`game-internal-api.ts:20`).
pub const STARSYSTEM_PATH: &str = "/2.0/elite/starsystem";
/// `EDDN_UPLOAD_URL` (`game-internal-api.ts:30`) — the non-standard port and the
/// trailing slash are both required by EDDN's `docs/Developers.md`.
pub const EDDN_UPLOAD_URL: &str = "https://eddn.edcd.io:4430/upload/";
/// `EDDN_SOFTWARE_NAME` (`game-internal-api.ts:32`).
pub const EDDN_SOFTWARE_NAME: &str = "edm";
/// `EDDN_SOFTWARE_VERSION` (`game-internal-api.ts:34`) — EDDN requires this to be
/// incremented whenever the content of the messages sent changes.
pub const EDDN_SOFTWARE_VERSION: &str = "1.0.0";
/// `EDDN_GAME_VERSION` (`game-internal-api.ts:36`).
pub const EDDN_GAME_VERSION: &str = "GameInternal-Live-market";

// The four numeric defaults are `f64` because that is what a JavaScript
// `number` is, and because their consumers do `f64` arithmetic on them
// (`Math.max(1, Math.min(MAX_CONCURRENCY, ...))`, `timeout * 1_000`). They
// reach the help text through `js_number`, exactly as `${...}` would.

/// `DEFAULT_CONCURRENCY` (`game-internal-api.ts:38`) — a sweep is a pool of
/// workers pulling from one queue, not a fixed rate.
pub const DEFAULT_CONCURRENCY: f64 = 5.0;
/// `MAX_CONCURRENCY` (`game-internal-api.ts:39`).
pub const MAX_CONCURRENCY: f64 = 16.0;
/// `DEFAULT_TIMEOUT_SECONDS` (`game-internal-api.ts:40`).
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 10.0;
/// `DEFAULT_REQUEUES` (`game-internal-api.ts:41`).
pub const DEFAULT_REQUEUES: f64 = 3.0;

/// The `USAGE` text (`game-internal-api.ts:1029-1117`), rendered.
///
/// Printed on **stdout** — including on the two exit-2 paths, where the
/// diagnostic goes to stderr and the help text does not \[R49\]. It carries no
/// trailing newline of its own; the caller's `console.log` supplies one.
#[must_use]
pub fn usage() -> String {
    let default_concurrency = js::js_number(DEFAULT_CONCURRENCY);
    let max_concurrency = js::js_number(MAX_CONCURRENCY);
    let default_timeout_seconds = js::js_number(DEFAULT_TIMEOUT_SECONDS);
    let default_requeues = js::js_number(DEFAULT_REQUEUES);
    format!(
        r#"game-internal-api.ts — Elite Dangerous game-internal API client

Usage
  bun game-internal-api.ts [command] [options]

Commands
  market [name]            {MARKET_LIST_METHOD} {MARKET_LIST_PATH} — one market's commodity listing, or every
                           market in a system when given a system name (default command)
  list                     alias for market
  trade                    {MARKET_TRADE_METHOD} {MARKET_TRADE_PATH} — buy or sell one commodity
  markets <name>           {STARSYSTEM_METHOD} {STARSYSTEM_PATH} — resolve a system or station name
                           through Ardent and list the market ids in that system
  help                     this text

Credentials (option, else environment)
  --cmdr-id       COMMANDER_ID    --machine-id     MACHINE_ID
  --machine-token MACHINE_TOKEN   --auth-token     AUTH_TOKEN   (80 / 2024 chars)

Shared options
  --market-id <id>         market to talk to (else MARKET_ID)
  --nonce <hex12>          fixed 12-hex nonce instead of a fresh random one per request
  --f-time <unix>          override fTime          --request-time <ms>  override Request-Time
  --fdev-semver <v>        default 4.4.0.3         --fdev-season <n>    default 4
  --user-agent <ua>        default EDGame/11.0/Win64
  --method <verb>          override the verb (list uses {MARKET_LIST_METHOD}, trade uses {MARKET_TRADE_METHOD})
  --dry-run                resolve and show the request without sending it. For trade the
                           read-only price lookup still runs; add --no-resolve to stay offline
  --full-url               print the encrypted query in full
  --json                   emit JSON instead of tables (for piping)

trade options
  --type buy|sell          required
  --item <id|name>[,...]   one commodity, or a comma-separated list worked in the order given
  --qty <n>                units per commodity (required unless --fill)
  --cargo <n>              hold capacity; buys are clamped to the space left
  --fill                   buy until the hold is full, spending down the --item list in order
  --watch                  repeat until the hold is full (needs --fill or --attempts)
  --interval <seconds>     delay between rounds, default 1
  --attempts <n>           stop after n rounds; 0 (default) means only --fill stops the loop
  --credits <n>            starting balance, so the first buy can be sized to it; otherwise
                           the balance is only known after the first trade replies
  --unit-price <n>         price per unit; taken from the market when omitted
  --final-qty <n>          defaults to --qty (single trades only)
  --black-market           force the black-market flag (default: on for stolen or illegal goods)
  --stolen                 mark the goods as stolen (default off)
  --no-resolve             never prefetch {MARKET_LIST_PATH}; requires numeric --item and --unit-price
  --no-cap                 send --qty verbatim instead of clamping it to stock / holdings
  --full-market            also print the whole commodity table from the trade response

market options
  [name]                   system name: sweeps every trading market in it
  --market-id <id>         a single market instead (else MARKET_ID)
  --concurrency <n>        parallel workers for a sweep, default {default_concurrency}, max {max_concurrency}
  --timeout <seconds>      per-attempt timeout, default {default_timeout_seconds}
  --requeue <n>            requeue a timed-out or transient failure this many times,
                           default {default_requeues} (EDDN posts are never retried in-run)
  --detail                 print the full commodity table for every market in a sweep
  --all-markets            include markets with nothing listed as imported or exported
  --carriers               include fleet carriers
  --eddn                   publish each market to EDDN ({EDDN_UPLOAD_URL})
  --eddn-test              same, but against the /test schema, which is not relayed onward
  --uploader <name>        EDDN uploaderID; defaults to the commander id
  --game-version <v>       default {EDDN_GAME_VERSION}    --game-build <v>  default empty
  --software-name <n>      default {EDDN_SOFTWARE_NAME}   --software-version <v>  default {EDDN_SOFTWARE_VERSION}
  --horizons / --odyssey   set only if you know them; omitted entirely otherwise
  --system <name> --station <name> --station-type <t>
                           name a single --market-id for EDDN when Ardent cannot

markets options
  <name>                   system or station name; quote anything with spaces
  --system <name>          treat the name as a system only
  --station <name>         treat the name as a station and use its system
  --address <id64>         skip Ardent and use this systemAddress
  --language <code>        default en          --cached-timestamp <n>  default 0
  --carriers               include fleet carriers (hidden by default; there are often hundreds)
  --trading                only markets that actually buy or sell commodities
  --dump <file>            write the decoded starsystem payload for inspection

Examples
  bun game-internal-api.ts market --market-id 4306502403
  bun game-internal-api.ts market Colonia --eddn
  bun game-internal-api.ts market --market-id 128667761 --eddn-test
  bun game-internal-api.ts markets "Hyades Sector NI-X a16-0"
  bun game-internal-api.ts markets --station "Jaques Station"
  bun game-internal-api.ts list --market-id 4306502403
  bun game-internal-api.ts trade --market-id 4306502403 --type buy --item silver --qty 10
  bun game-internal-api.ts trade --type sell --item 128049155 --qty 5 --unit-price 3340 --stolen
  bun game-internal-api.ts trade --type buy --item palladium,gold --cargo 1232 --fill
  bun game-internal-api.ts trade --type buy --item palladium,gold --cargo 1232 --fill --watch"#
    )
}
