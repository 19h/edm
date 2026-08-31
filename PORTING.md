<!-- The contract between game-internal-api.ts and this port. -->

`game-internal-api.ts` is the specification. Every observable byte on stdout and
stderr, every exit code, and every byte on the wire is reproduced unless it
appears below marked **CORRECT**. Anything that differs and is not listed here
is a bug.

Rows are referenced from code comments and test names by their `R`/`C` number,
so renumbering them is a breaking change to the test suite.

## Status

| Step | Module | State |
|---|---|---|
| 1 | `edm_core::js` | done — 7 oracle fixtures green against bun 1.2.3 |
| 2 | `edm_core::js::json` | done — key order and round-trip green over 614 documents |
| 3 | `edm_core::wire` | done — 18 LZ4 vectors + 4 RFC 8439 vectors, Bun-golden |
| 4 | `edm_core::render` | done — 90 Bun-blessed snapshots |
| 5 | `edm_core::cli` | done |
| 6 | `edm_core::domain` | done — including the batch state machine |
| 7-10 | `edm` I/O layer | done — sys/secret/ports/net/game_api/exchange/ardent/eddn/out |
| 11-12 | sweep and commands | done — plus the four extension commands `route` (C25), `eddn` (C33), `vendor` (C35) and `sell` (C41) |
| 13 | `cargo xtask parity` | **green — 65 of 65 differential scenarios byte-identical, plus 17 `route` scenarios diffed against goldens (C25)** |
| 14 | `edm_core::pace` / `edm::route` | done — pacer, two-stage pool, spend gate, resumable cache |
| 15 | `edm-route` | done — the optimiser: exact-integer arithmetic, Dinkelbach max-ratio cycle, brute-force oracles |
| 16 | first live run | done — `route Sol --radius 10`: 22 markets, 22 requests, 7.5 s against a 6 s estimate |

## Known gaps

Opt-in behaviour that is registered but not implemented. None affects the
default path, and none is exercised by the harness.

- **`EDM_STRICT_JSON=1`** is honoured at the command layer but not for the five
  diagnostics inside `exchange::send`, which write through `Out` and would need
  it to carry the flag.
- **C23** — `--method connect/trace/track` and a non-ASCII `--user-agent` are
  not rejected. No message was ever pinned for them.
- **C9 (`EDM_EDDN_ONCE=1`)** and **`EDM_EDDN_BRACKET=passthrough`** have no
  implementation; the second needs a slot in `eddn::build_message`.
- **R86 reports the clamped delay.** `timeout_failure` reads a `Duration`, so a
  `--timeout` above `INT32_MAX/1000` prints `1 ms` where the original prints the
  raw value. C22's clamp itself is implemented.

## What the first live route run found

`edm route Sol --radius 10 --max-requests 200 --cargo 784`, 2026-08-05: 13
systems in radius, 466 stations known to Ardent, **22 markets after the default
filter** — 254 fleet carriers, 127 Odyssey settlements and 61 outposts removed,
95% of the region. 22 requests in 7.5 s against a 6 s estimate. Every reported
route carried `proved optimal`.

Three defects it found, all now fixed and all with a test:

1. **`Claim` was dropped to fit an ordinary 100-column terminal**, so the run
   printed twenty credits-per-hour figures with nothing said about what was
   proved of any of them. `Rate` and `Claim` both declare priority zero now —
   the rule `Route::rate` enforces in Rust, carried into the table.
2. **The second run was served entirely from the cache and still printed "every
   price below was read live from the game-internal API during this run".**
3. **That run's plan priced 22 requests and then sent none**, because the cache
   was consulted after the spend gate rather than before it.

And one in the harness: scenarios shared a cache through the developer's real
`~/.cache`, so `route-ceiling-refuses` began proceeding past its ceiling once an
earlier scenario had cached one of the two markets it counted. Each side now
gets an `XDG_CACHE_HOME` under its own scenario directory.

## Measured facts about the game-internal API

- **`/2.0/elite/market/list?marketID=N` answers for a market the commander is
  not docked at.** Measured 2026-08-05 against four real markets — Prince
  Terminus (Hyades Sector NI-X a16-0), Mourelle Gateway (G 65-9), Galileo (Sol)
  and Jaques Station (Colonia). All four returned HTTP 200 with distinct
  payloads, and each one's highest sell price matched Ardent's independent
  crowd-sourced record for the same market **exactly**: 194,908 / 476,614 /
  476,614 / 599,184. Four independent agreements is identity, not coincidence.

  This is what the sweep has always assumed and nothing had ever checked. It is
  also the premise the whole `edm route` feature rests on.

  Two incidental findings from the same measurement. The payload has **no `id`
  field** — its top-level keys are `allowsDumping`, `commodities`, `inventory` —
  so a port cannot verify which market answered by reading the response; the
  only check available is that the *content* differs per id, which is the
  stronger test anyway. And every market returns the same 391-entry commodity
  map, most rows priced but with zero stock and zero demand, so a commodity
  count is not a proxy for how much a market actually trades.

### Fleet-carrier endpoints, and where docking access actually lives

Read out of a Frida capture of a live session (`outx.log`, 2026-08-26; it grows
while the game runs, so a line count is not a stable citation). None of these is
called by this program; they are recorded because C36 turns on which of them is
affordable.

| Endpoint | Parameters | Answers |
|---|---|---|
| `2.0/elite/fleetcarrier/galaxymap` | `x`, `y`, `z`, `radius` (200 observed) | a bare array of system addresses that contain a carrier — no per-carrier detail |
| `2.0/elite/fleetcarrier/system` | `systemAddr` | every carrier parked there: `market_id`, `body_site_id`, `name.callsign`, `location`, `owner`, `squadron`, `state`. Up to 37 in one reply |
| `2.0/elite/fleetcarrier/info` | `fleetCarrierId`, and `cmdrId`, `language=en` | one carrier in full, **including `docking.accessLevel` and `docking.notoriousAccess`**, and an echo of both `body_site_id` and `market_id` |
| `2.0/elite/fleetcarrier/systemmap` | `systemAddr` | parking slots for the commander's *own* carrier. Nothing about anyone else's |

**`fleetcarrier/system` has no `docking` member at all** — every one of its 104
carrier records across 15 replies carries the same nine keys, `body_site_id`,
`loadout`, `location`, `market_id`, `name`, `owner`, `squadron`, `state`,
`version`, and `docking` is not among them. (An earlier revision of this section
said the key was present and null. It was measured with a lookup that returns
the same value for an absent key as for a null one, which is the mistake this
paragraph now exists to prevent: a parser written to that description would wait
for a key that never arrives.) So the bulk endpoint cannot answer the question,
and access must be fetched one carrier at a time.

**`fleetCarrierId` is arithmetic, not a lookup.**
`market_id = fleetCarrierId * 256 + 3_290_400_000`, exact over all 157 id pairs
in the capture (53 `/info` replies and 104 `/system` records), zero exceptions.
So `/info` is reachable straight from a market id and the `/system` round trip
is not needed at all. A market id that is not congruent — below the base, or off
the 256 stride — is not a carrier's, which makes the arithmetic its own filter.
Confirmed live 2026-08-26 against `3711014400` → `1643025` → `T1N-W2F`,
`accessLevel: "squadronfriends"`, with the reply echoing `market_id` back
unchanged.

**A carrier that does not exist answers HTTP 204 with an empty body**, not a
4xx. Measured against two synthetic ids. So a decommissioned carrier still
listed by Ardent is distinguishable from a failure, and from a live one.

`accessLevel` over 53 replies covering 52 distinct carriers in 6 systems:
`all` 31, `squadronfriends` 13, `squadron` 5, `friends` 4. **Lowercase, one
token** — `squadronfriends`, not Spansh's `Squadron Friends`. No `none` was
observed, but the journal enum has one and it must never be conflated with an
absent value: `none` is the *strictest* carrier in the game and unknown is the
least informative state there is.

`/info` was observed answering for carriers the commander does not own, in
systems the commander was not in, so it is not proximity-gated. It also returns
that carrier's `finance`, `inventory` and `crewRoster`.

The journal's `Market` event carries `CarrierDockingAccess` (lowercase `"all"`
observed), which is what EDDN's `commodity/3` renames to `carrierDockingAccess`
and therefore what Spansh ultimately indexes. It fires only when the commander
opens the market **at** a carrier — after docking — so it cannot filter
anything; it is the mechanism that makes Spansh's coverage exactly "carriers
somebody has recently docked at".

## Measured facts about Spansh

Third-party, and recorded separately from Frontier's own endpoints for that
reason. Measured 2026-08-26.

- **`carrier_docking_access` is the cheap source, not the only one.** Frontier
  answers the same question authoritatively at one metered request per carrier;
  see the fleet-carrier endpoints above. Spansh's values are exact and
  case-sensitive: `All`, `Squadron`, `Friends`,
  `Squadron Friends`, `None` — or the key is **absent**, which is a third state
  and not a default. `["SquadronFriends"]` and `["squadron friends"]` both match
  nothing, and matching nothing is spelled the same way as "nothing is
  restricted", which is why the five strings are `const` and pinned by a test.

- **Roughly a third of carriers publish nothing.** Over the population
  `route --carriers` actually ranks — every carrier Ardent lists in the radius,
  because `select::reject` applies no freshness test — 528 carriers across 12
  systems in 3 regions gave 70.6% published / 13.8% provably restricted / 29.4%
  unpublished; an independent 240-carrier set across 12 other systems gave
  65.0% / 8.8% / 35.0%. The shape is stable and the exact fractions are not.
  Restricted and open carriers have statistically indistinguishable staleness,
  so absence is not recoverable by heuristic.

- **`POST /api/stations/search` answers HTTP 200 to several malformed
  requests**, and none of them is visible from the status:
  - `size` above 500 is silently replaced by **25**, and 25 rows come back;
  - a **misspelled filter key** is ignored rather than refused — the reply is
    the whole unfiltered id set, and the server echoes the misspelling back
    under `search`, so the echo validates nothing;
  - `from + size > 10000` is HTTP 500, which is why a batch never pages.

  Every one of these would read downstream as "fewer carriers are restricted
  than really are". Each has a guard in `edm_core::spansh`.

- **`market_id` filters accept a batch as an array of id strings**, and the
  reply carries `market_id` as a JSON *number*. Only that field and
  `carrier_docking_access` are read: Spansh's `system_name` disagreed with
  Ardent's for 99 of 528 carriers (18.8%) because carriers jump, and its
  `distance` is measured from Spansh's own default reference rather than the
  run's centre, so admitting either would silently mix two reference frames.

- **No rate limiting was observable** across roughly 60 requests from two
  independent measurements — no 429, no `Retry-After`, no `RateLimit-*`
  headers. Absence of an enforced limit is not permission; the concurrency is 4
  and the realistic worst case is a handful of requests.

## Measured deviations from the design

Recorded when an assumption made while planning turned out to be wrong.

- **The trade graph is dense, and `graph.rs` said the opposite.** Its module
  comment justified the commodity-major build with "most market pairs share no
  tradeable commodity at all". The pivot is right and the reason was wrong.
  Measured 2026-08-06 over the 22 real market payloads the first live run
  cached: `Σ_c |suppliers| · |buyers|` is **7,038** against a pair-major
  180,642, so commodity-major is 25× cheaper — but **410 of the 462 ordered
  pairs, 89%, have a profitable trade between them**, and over a cached
  5,049-market sweep it is **95%**. Three things follow, and all three were
  costing whole minutes:

  - the build is quadratic in the market count with no help from the data —
    **127 s and 24,292,232 legs at 5,049 markets**, in silence, with a
    transient peak of **4.1 GiB**;
  - the decomposition is one component holding 5,045 of those 5,049 nodes, so
    `ratio::Component` was copying the whole edge list to re-index it into
    something nearly identical;
  - one Bellman-Ford probe cannot exit early while a positive cycle exists —
    the early exit *is* the stopping condition — so every Dinkelbach round but
    the last costs `n·m` relaxations, 1.2e11 of them here. On a comparable
    5,000-market instance where the iteration did improve, one round took
    **205 s**.

  Fixed by addressing the graph's edge arrays instead of copying them
  (556 MiB → 278 MiB of component, and a measured 2,561 → 2,299 MiB peak for
  the search phase), hoisting the reduced-weight buffer out of the round loop,
  and giving the search a caller-supplied wall-clock budget and progress sink
  (`edm_route::watch`). The pure crate has no clock, so the budget is a
  predicate the caller answers; `edm_route::watch` records why a step counter
  could not have stood in for one.

  Two claims in the investigation that prompted this did **not** survive
  measurement. The build was estimated at 10-48 s and is 127 s. And the
  single-component shortcut it asked for does not fire on real data at all:
  four of the 5,049 stations trade with nobody, so `sccs()` returns five
  components, not one. The fix is the general one; the shortcut is kept because
  it is four bytes an edge cheaper when it does fire.

- **This tree is hand-formatted and there is no `rustfmt.toml`.** `cargo fmt`
  rewrites 68 files and ~4,300 lines, and no simple configuration round-trips
  the committed layout — `use_small_heuristics = "Max"` collapses some
  expansions and expands others. A config file that did not reproduce the tree
  would be a trap for the next person, so there is none. **Do not run
  `cargo fmt` here**: a formatting sweep in one branch makes every other branch
  conflict, and this repository's value is a byte-for-byte comparison whose
  diffs have to stay readable. Making the tree rustfmt-clean is a defensible
  decision; it is a commit of its own, not a side effect.
- **The plan's starsystem-per-system read is off by default.** §3.1 has the
  game-internal API's `starsystem` payload confirm what Ardent proposed, one read
  per system. Step 0 then established that the game-internal API answers for a
  market the commander is not docked at, which makes Ardent's market ids usable
  directly — and a starsystem payload is ~500 KB against a market's ~20 KB, with
  roughly one starport per system near Sol. Reading one per system to
  rediscover ids already in hand is twenty-five times the transfer for the same
  prices. It is `--verify-systems`, and what it buys — a market Ardent has never
  seen — is stated rather than paid for by default.
- **The parity harness's `env_clear()` did not clear the environment.** Bun
  loads `.env` from its working directory before the script runs, and the
  harness runs both sides in the repository root. The moment a `.env` existed
  there for the step-0 live check, two scenarios began failing: the Bun side
  picked up a `MARKET_ID` the Rust side never saw, so `market --no-json` sent a
  request on one side and reported a missing market id on the other. The
  harness was inventing a divergence rather than finding one — and a live
  `AUTH_TOKEN` reaching a side of a differential test is the worse half of the
  same hole. Measured directly: `--env-file` replaces Bun's default set
  outright, so naming an empty file loads nothing. Every `bun` invocation in
  `xtask` now passes `--env-file xtask/oracle/empty.env`, and the
  `no-dotenv-leakage` gate fails the build if one does not.

- **C13 was too weak.** The plan proposed an ASCII-only collation model with
  non-ASCII falling back to scalar order. Measurement killed it: `ß` expands to
  `ss`, `þ` sorts after `z`, `ø` is a secondary variant of `o`, `æ` groups with
  `a`, and `½` falls between `1` and `2` — and Elite Dangerous names contain all
  of these. `edm_core::js::collate` now delegates to ICU4X (the same CLDR data
  JavaScript engines use) and is pinned by a 7,279-row corpus. The
  `icu-collation` cargo feature is therefore unnecessary and does not exist.
- **`EDM_WIDTH=display` is a runtime `Metric`, not a cargo feature.** Cheaper to
  test and it keeps one binary.
- **The `fit_commodity_48` expectation in the plan was wrong.** It named surviving
  widths of `[30, 6, 6]`; the answer is `[18, 10, 10]`, confirmed against Bun.
  `[30, 6, 6]` is unreachable at any content, because `frameWidth` is
  `sum(w) + 3n + 1` — three columns at 30/6/6 frame to 52, not 48, and
  `Commodity` still has 18 units of slack for the loop to squeeze. The slip was
  counting one separator per column instead of three.
- **`renderTable` can return a frame wider than the terminal.** When nothing is
  shrinkable (R27's slack rule leaves a column with no `minWidth` unable to
  shrink) and nothing is droppable, the loop breaks and the table overflows.
  R27 describes the loop but not this failure mode. Reached in practice by
  `INVENTORY_COLUMNS`, whose `Qty`/`Value`/`S` declare no floors.
- **C11 is narrower than the bug.** `COLUMNS` of twenty digits yields `1e20` and
  a `RangeError: Invalid count value`; `COLUMNS` of four hundred digits yields
  `Infinity` and a *different* `RangeError`. Both happen at module init, outside
  `main`'s try/catch. The single clamp covers both.
- **R71 is wrong.** A repeated *response* header does not combine with `", "`
  under bun 1.2.3 — it **overwrites**. Two `uncompressedsize` headers of `512`
  and `4096` yield `4096`, so the body is decrypted at the wrong size and
  refused by the LZ4 length check rather than by the header check; two `allow`
  headers on a 405 yield only the last. The WHATWG `Headers` class does specify
  combining, and a port written from the specification gets this wrong.
  Measured with a raw socket server; `HeaderView::get` is last-wins.
- **R66's Content-Type half is wrong.** A `""` body on PUT sends
  `Content-Length: 0` and **no** `Content-Type`. Measured the same way. (reqwest
  omits the length entirely for an empty body, so it has to be set by hand.)
- **C11 is wrong twice over.** The failure is not a `RangeError` and not at
  module initialisation: `TERMINAL_WIDTH` computes fine, and nothing goes wrong
  until the first `heading` calls `"=".repeat(1e20)`, at which point Bun prints
  a bare `Out of memory` and exits 1. The clamp is still right; the description
  was not, and the scenario asserting it ran `help`, which never renders a
  heading and so could never have shown the divergence.
- **Content coding is done by hand, not by reqwest.** reqwest decompresses and
  then removes `content-encoding` and `content-length` from the response
  headers; `fetch` leaves both visible, and this program prints the header
  table.
- **The game-internal API origin is an argument to `game_api::prepare`, not a constant
  it reaches for.** Building with the constant and rewriting the prefix
  afterwards is a step a caller can forget, and one did: the sweep sent every
  market poll to the live game-internal API while the harness believed it was
  talking to a mock. The signature now cannot be satisfied without deciding.
- **R47 swallows exactly two tokens, and the reason is not the obvious one.**
  The lookup is `BOOLEAN_LITERALS[next.toLowerCase()]`, so the token is folded
  *before* the property access — `toString` becomes `tostring`, which is not a
  key on `Object.prototype`. Only `constructor` and `__proto__` are already
  lowercase and survive the fold. Widening the set to every prototype member
  would swallow tokens the original leaves as positionals.
- **R47's message, measured:** a poisoned switch throws
  `value.toLowerCase is not a function. (In 'value.toLowerCase()', 'value.toLowerCase' is undefined)`
  under Bun 1.2.3, and exits **1**. Recorded in `cli_errors.tsv` and asserted
  against `cli::POISON_TYPE_ERROR`.
- **C5's Bun string, measured:** `TextDecoder("utf-8", {fatal: true})` under Bun
  1.2.3 raises `TypeError: Invalid byte sequence`.
- **C4's error text, now fixed:** `Cannot allocate {n} bytes for the decompressed
  response`, with the cap at 256 MiB.
- **R59 is incomplete.** `decodeOpaqueBody` also `.trim()`s its input (ts:2820),
  sharing `decryptResponse`'s `js_trim` prefix including U+FEFF. R59 lists three
  deviations and omits this one.
- **R18 covers three of four optional summary rows, not four.** `credits`, `debt`
  and `allowsDumping` are presence probes; `lastModified` is gated on `asRecord`
  (ts:736), so `"lastModified": 1700000000` omits the row entirely while
  `"credits": null` still prints `0 cr`.
- **R33's clamp is only about progress lines.** Two emitters print verbatim — the
  full URL under `--full-url` (ts:1195) and a decoded non-market body (ts:1296).
  `Block::Raw` exists for them; routing a kilobyte of base64 through
  `Block::Line` would render it as a single `~`.
- **R94's ladder starts three steps earlier than stated.** `--market-id`,
  `--type` and `--item` are each a `requireValue` that can throw, and they are
  ordered too: `trade --type nonsense --qty 0` reports the *missing market id*.
- **`derivePrice` precedes the stock clamp.** A commodity with `buyPrice 0` *and*
  `stock 0` reports "not sold at this market", never "stock is 0". Neither R91
  nor R94 gives their relative order.
- **`--cargo` is read *inside* the `commodity && capQty` branch**, so
  `--no-cap --cargo abc` and every `--no-resolve` path never validate it at all.
  R94 lists "parse cargo" as an unconditional step.
- **ts:1847 is dead code.** The ts:1829 guard already requires `--unit-price`
  whenever there is no commodity, so `Could not determine a unit price` is
  unconstructible. Transcribed anyway rather than added to C18.
- **R47's poison is reachable only past a short-circuit.** `runTrade`'s `||`
  chain and `runMarketSweep`'s `wantsEddn` both skip the poisoned slot when an
  earlier operand is true: `trade --item a,b --fill constructor` succeeds where
  the single-item form exits 1.
- **Two batch guards are wider than they read.** `--qty must be at least 1` is
  not gated on `!fill`, so `--fill --cargo N --qty 0` is rejected even though
  `--qty` is otherwise ignored; and `--no-resolve cannot be used with --fill or
  multiple items` fires for *every* batch run, naming two situations the guard
  does not test for.
- **`markets` reads `--address` before the missing-name check**, so
  `markets --address abc` with no name reports the address, not the name.
- **`formatBracketMeter(NaN)` yields `""`**, not `"..."` — both `repeat` calls
  receive NaN. Unreachable, since the brackets come through `readNumber`, but a
  divergence in waiting if anything ever passes a raw value.

# Parity register

**REPRODUCE** = match the TS byte-for-byte. **CORRECT** =
deliberate, registered divergence with an allowlist row in the harness. Anything not listed that
differs is a bug.

### A.1 REPRODUCE — numbers, JSON, coercion

| # | Behaviour |
|---|---|
| R1 | Number→string is ECMA `Number::toString` everywhere: `1e21`→`"1e+21"`, `-0`→`"0"`, `10.0`→`"10"`. Never `f64: Display`. |
| R2 | All JSON numbers are `f64`. `JSON.parse` rounds >2⁵³; `stringify` re-emits shortest-round-trip. |
| R3 | Integral doubles serialize with no `.0`. **This is the EDDN gate.** |
| R4 | `JSON.stringify(NaN｜±Infinity)` → `null`. |
| R5 | Key order: array-index keys (canonical decimal, ≤2³²−2) ascending numerically first, then insertion order. Duplicates: last value, first slot. |
| R6 | `undefined` values omitted; object spread keeps a key's *original* position (`{...settings, items}` leaves `items` 3rd). |
| R7 | `formatInteger` = `Math.trunc` then `toLocaleString("en-US")`: `-0`→`"-0"`, never exponential, zero-padded above 2⁵³, non-finite→`"?"`. Must survive ~1e31 (`unitPrice * qty`). |
| R8 | `formatQuantity` tests `=== 0` **before** truncating: `-0`→`"-"`, `0.4`→`"0"`. |
| R9 | `toFixed(1)` rounds ties away from zero (`0.25`→`"0.3"`); `-0`→`"0.0"`; `\|v\| ≥ 1e21` → `Number::toString`. |
| R10 | `Number(s)`: `null`/`""`→0, `" 12 "`→12, `"0x10"`→16, `"1e3"`→1000, `"Infinity"`→∞; rejects `"inf"`, `"1_0"`. Governs `uncompressedsize`, `--interval`, `--timeout`, `COLUMNS`, `Number(key)`. |
| R11 | `parseUnsignedInteger`: ASCII `/^\d+$/` (rejects `"１"`), then `isSafeInteger`; the two messages differ and a 100-digit string gets the *second*. |
| R12 | `Math.round` = half toward +∞ (`-2.5`→`-2`), `-0` preserved, `0.49999999999999994`→`0`. |
| R13 | `Math.min`/`max` propagate NaN and specify ±0. |
| R14 | `isSafeInteger` = `fract()==0 && abs()<=2^53-1`. Never `x as i64 as f64 == x` (saturates). |
| R15 | `>>> 0` **wraps** mod 2³² (`--request-time 4294967296` → 0). Never `as u32`. |
| R16 | `readNumber(..) \|\| Number(key) \|\| 0` uses JS truthiness. `readMarketPoints` has only **two** falls, not three (verified). |
| R17 | `readNumber`→0 for missing/non-number/non-finite; `readString`→`""`; `readBoolean` = `=== true \|\| === 1`; but `consumer`/`producer`/`rare` use `> 0`, so JSON `true` yields **false**. `illegal` = non-empty `legality` *string*. |
| R18 | `"k" in o` probes: `credits: null` → `Some(0.0)`, which clamps every buy to zero via `floor(0/price)`. `Option<f64>` would disable the affordability clamp and spend money the commander lacks. |
| R19 | `lookupFaction`: direct `factions[String(id)]`, then a linear rescan comparing `Number(key) === wanted`. The rescan is live *because* of R2. |
| R20 | `toISOString`: exactly 3 fraction digits, `Z`, expanded `±YYYYYY` outside 0..9999; `\|ms\| > 8.64e15` → Invalid Date → bare `js_number(seconds)`. |
| R21 | `formatUnixSeconds` interpolates `${seconds}` **ungrouped**; `formatMilliseconds`'s `padStart(2,"0")` does not truncate. |

### A.2 REPRODUCE — text and rendering

| # | Behaviour |
|---|---|
| R22 | `String.length`/`slice`/`padEnd` are **UTF-16 code units** throughout the renderer. |
| R23 | `clampText` order: `<= width`, then `width === 1`→`"~"`, then `slice(0,w-1)+"~"`; `width <= 0`→`""`. |
| R24 | Truncation bisecting a surrogate pair emits **U+FFFD**. |
| R25 | `.trim()` set = Unicode White_Space ∪ {U+FEFF}; `Number()`'s `StrWhiteSpace` and regex `\s` = the same **minus U+0085**. Two predicates, both oracle-pinned. |
| R26 | `localeCompare`: punctuation < digits < letters, case-insensitive primary, lower before UPPER at tertiary. Sorts are **stable** (ES2019). |
| R27 | `renderTable`: shrink the *first* max-slack column; drop the *first* max-priority column (`reduce` with strict `>`); widths recomputed from scratch after each drop; `slack = w − (min ?? w)` so a column without `minWidth` **cannot** shrink; `maxWidth` applied before the `minWidth` floor. |
| R28 | `heading` uses `>=` (a label exactly `W` wide gets no `=`); titles contain U+2014, which is **1** unit. |
| R29 | `emitNote` splits on a *single* space: empty tokens preserved, a leading space dropped, an over-long word overflows unbroken, `emitNote("")` prints nothing. Limit `max(20, W-3)` measured *without* the indent. |
| R30 | `previousWasRule`: leading rule/band after the header rule emits no dash, consecutive rules collapse, a band always trails a dash, the closing dash appears only after a data row. Band width `max(1, frame-4)`; bands do **not** influence column widths. |
| R31 | `TERMINAL_WIDTH`: `COLUMNS` (js-trimmed, `/^\d+$/`) first, else the **fd-1** ioctl, else 100 (not 80); floor 48 on both paths; sampled once at startup, SIGWINCH ignored. |
| R32 | `toLowerCase`/`toUpperCase` are full Unicode (`"straße"`→`"STRASSE"`), so band rows can lengthen. But the POI type regex `/…/i` without `u` is **ASCII**-case-insensitive. |
| R33 | Progress lines are `clampText`ed to `TERMINAL_WIDTH`. |
| R34 | `elide(s, h, 0)` returns the whole string (`slice(-0)`). |
| R35 | The sweep `EDDN` cell is `clampText(detail, 24)` *before* measurement, so `~` can appear in a narrower column. |
| R36 | Hidden-columns note uses the raw ungrouped width and a `", "` join. |
| R37 | `--dump` reports `decrypted.length` — UTF-16 units — labelled "bytes". Wrong, cosmetic, reproduced. (Line 1187 *is* genuinely UTF-8 and maps to `plaintext.len()`.) |

### A.3 REPRODUCE — CLI

| # | Behaviour |
|---|---|
| R38 | `--` is `Unknown option --`, **not** an options terminator. `-h` is the only short flag, case-sensitive. |
| R39 | A bare switch consumes the next token iff its lowercase form is a boolean literal (`--detail 1` consumes; `--dry-run Colonia` does not). |
| R40 | `--no-` matched `/^no-/i` on the **raw** name *before* `[-_]` stripping (`--no-json` works, `--no_json` is `Unknown option`); switches only; the error interpolates the whole raw name; any `=value` is discarded. |
| R41 | Normalisation: strip *all* `-`/`_`, then **full-Unicode** lowercase (`--mar\u{212A}etid` works). Never `to_ascii_lowercase`. |
| R42 | Only the first `=` splits (`--item=a=b` → `"a=b"`). |
| R43 | `--qty=` stores `""` (falls through to the env); `--json=` is a parse error. Asymmetric. |
| R44 | A value flag accepts a next token starting with a single `-`, not `--`. |
| R45 | An empty argv token does **not** fill the command slot (`edm "" Colonia` → `Unknown command "colonia"`). Model `command` as `String` with `""`, never `Option`. |
| R46 | Parse errors name the **alias you typed** (`--market requires a value`); accessor errors name the **canonical display** (`--capacity abc` → `--cargo must be an unsigned decimal integer`). |
| R47 | `BOOLEAN_LITERALS["constructor"｜"__proto__"]` resolve through `Object.prototype`: the token **is** consumed, the flag holds a function, and `optionalSwitch` later throws → **exit 1**. A naive port leaves it a positional and exits 2. Modelled as `Value::Poison`. |
| R48 | The `help` **command** is checked before the `--help` **switch**; both before the known-command set (`edm bogus --help` → exit 0). |
| R49 | Exit-2 messages emit a **blank line** after them on stderr; `USAGE` goes to **stdout**. |
| R50 | Read order is lazy and observable: stamp flags validated per request; sweep settings read **after** two network calls; the batch ladder's 16 steps in order (market-id **last**); credentials loaded for every command including `markets --dry-run`, ASCII-then-length per field in source order. |
| R51 | `--concurrency 0` → **1** worker; `--timeout`/`--requeue` unbounded; `--cached-timestamp` honoured by `markets`, hardcoded 0 by the sweep. |
| R52 | Name precedence is **opposite** between commands (verified): `market` = `--system ?? --station ?? positional`; `markets` = `--station ?? --system ?? positional`. `market-id` staging: flag-without-env, then name, then flag-with-env. |
| R53 | `trade --market-id` is **never parsed** (verified) — `0004306502403` goes on the wire verbatim, while `market --market-id 0004306502403` sends `4306502403`. |
| R54 | `runTrade` splits `--item` to choose single-vs-batch, but `resolveTrade` re-reads the **un-split** raw value. |
| R55 | argv and env decoded **lossily** (`args_os`/`var_os`); `std::env::args()` panics and must never be used. Env snapshot is **first-wins** per name. |
| R56 | `optionalValue` falls through to the env when the flag is present-but-blank, not only when absent. |

### A.4 REPRODUCE — wire and transport

| # | Behaviour |
|---|---|
| R57 | The nonce is the **12 ASCII characters**, not the 6 bytes they decode to. Request nonce lowercased; **response nonce used verbatim** (case is keystream-relevant). Two constructors, and a test proving they disagree. |
| R58 | Base64 gate `/^[A-Za-z0-9+/]*={0,2}$/ && len%4==0`, then a **lenient** decode accepting non-zero trailing bits. The gate accepts `""`, so an empty 2xx body reports `MissingFrame`, not a base64 error. |
| R59 | `decodeOpaqueBody` checks `compact === ""` **first**; its unframed path decodes the **whole** buffer with no 8-byte skip and never reads the size header. |
| R60 | Frame check is **length-first, then magic**; bytes 4..8 are never inspected. |
| R61 | LZ4: extend only on nibble `=== 15`, adding the terminating byte; `offset === 0` is an error; `offset > destination` uses the **running** destination; `+4` before the bound check; `break` when `source === input.length`; overlapping matches copy byte-at-a-time; final size check. All six error strings verbatim. |
| R62 | `Response.text()` = unconditional UTF-8 lossy decode + BOM strip, ignoring the `Content-Type` charset. Never `reqwest::Response::text()`. |
| R63 | The decompressed plaintext goes through `TextDecoder("utf-8",{fatal:true})`, which **removes** one leading U+FEFF. |
| R64 | Standard (not URL-safe), **padded** base64 appended raw to the query. No percent-encoding. |
| R65 | The envelope plaintext is UTF-8 **bytes**; `--language` is the one unvalidated field. |
| R66 | Body keyed on the **effective** method after `--method`: GET/HEAD → none; otherwise `""`, which makes fetch add `Content-Type: text/plain;charset=UTF-8` + `Content-Length: 0` **on the wire but not in the printed table**. |
| R67 | Redirects: game-internal API **none**; Ardent and EDDN **follow, limit 20** (not reqwest's default 10). |
| R68 | HTTP/1.1 only, both clients. |
| R69 | **No client-level timeout anywhere.** The sweep's per-attempt race is the only deadline, and it wraps the whole visit **including the EDDN POST**. A single `market --market-id` can hang forever. |
| R70 | `Fdev-Retry` is the constant `"0/2"` on every attempt, including requeues. |
| R71 | `Headers.get` joins duplicates with `", "` (two `uncompressedsize` headers → NaN → rejected). Iteration lowercased, sorted, duplicates combined. |
| R72 | `uncompressedsize` parsed with `Number()` then `isSafeInteger && > 0`; absent renders the literal `null`. The nonce message uses `JSON.stringify` (unquoted `null`, quoted `"abc"`). |
| R73 | 405: empty `Allow` is falsy → **no diagnosis line**; `Allow: ,` → `verbs[0]` undefined → the literal text `--method undefined` (verified); the message prints the **raw** header value. |
| R74 | The REQUEST table prints **before** the dry-run bail; `ignoreDryRun` paths still hit the network under `--dry-run`. The RESPONSE table prints exactly once, from one of two sites depending on `quiet`. |
| R75 | `process.exitCode` is **assignment**, last-write-wins, never reset; a non-2xx sets 1 and the run continues. Nothing calls `process::exit`. |
| R76 | `--json` failure diagnostics still go to **stdout** and corrupt the JSON stream; `--json` is guarded at ~20 individual sites, not globally. Opt-out: `EDM_STRICT_JSON=1`. |
| R77 | On the *single-market* path `--json` implies `quiet` (line 1579 passes `session.json` as the `quiet` arg), so the tables are correctly suppressed. Not a bug. |
| R78 | `--json` sweeps return before the failure tally, so they do **not** set exit 1 for markets with no usable data; non-`--json` sweeps do. |
| R79 | EDDN success is `status === 200 && trim(body) === "OK"` (**202 is a failure**); detail is `clampText(body, 120)`. Never retried in-run. |
| R80 | `encodeURIComponent`'s unreserved set (`A-Za-z0-9 - _ . ! ~ * ' ( )`) for Ardent URLs. |
| R81 | Ardent error handling is asymmetric: only the **first** system lookup is `.catch`-swallowed; `resolveStationByMarketId` swallows everything → `Ardent does not know market X`. |
| R82 | `main` prints `error.message` **alone** — no cause chain, no `{err:#}`, no `{err:?}`. |

### A.5 REPRODUCE — sweep and trade

| # | Behaviour |
|---|---|
| R83 | Enqueue order = R5 key order → progress-line order → `SWEEP RESULTS` row order. Workers dispatched in index order (worker *i* takes `targets[i]`). |
| R84 | Requeue to the **back**, retaining the attempt count; transient = `status ∈ {None, 408, 429, ≥500}` **and** `snapshot.is_none()` (a 2xx that fails to decrypt is `HTTP 200` → not transient). |
| R85 | `[requeue n/requeues]` uses `settings.requeues` as denominator (last line `3/3`, then attempt 4 retires). `[k/N]` counts retirements only, running exactly `1..=N`. |
| R86 | Timeout failure text — `timed out after 10,000 ms` vs `aborted (timeout)` — is **oracle-pinned by experiment**, not hand-written. |
| R87 | Under `--dry-run` a sweep leaves `failure` null → **no requeues**, every row `HTTP -  no data`, exit 1 via the "no usable data" path. The dry-run EDDN branch at line 1406 is unreachable dead code. |
| R88 | Duplicate market ids are polled twice and both rows show the **last** writer's result. |
| R89 | `MarketVisit.failure` is populated and never rendered. |
| R90 | Batch round-end checks in TS order — `hold is full` precedes `--dry-run: nothing was sent`. Sleep at the end of **every** continuing round, none after the last. Three consecutive failures required; `" n times in a row"` only when `n > 1`. A mid-watch snapshot failure **throws**: no TRADES table, no JSON, exit 1. The stamp is drawn **before** the dry-run branch (entropy-stream parity). |
| R91 | Trade clamps in `f64`: `floor(credits / unitPrice)` gated on `unitPrice > 0`; free space is `+Infinity` without `--cargo`; `qty = max(0, floor(qty))`; zero-qty reason chain is `available === 0` → `credits < unitPrice` → `"no cargo space"`. |
| R92 | `${intervalMs / 1000}s` and `${timeoutMs / 1000}s` go through `js_number` (`1`, `1.5`, `0.1`), never `{:.1}`. |
| R93 | `findCommodity` strips `/[\s_-]/g` from the **needle only**, so typing a full commodity name never matches. Two exact matches fall through to the partial branch. Ambiguity lists `slice(0,8)` with `", ..."`. |
| R94 | `resolveTrade`'s read/throw order: qty → reject 0 → lookup → the two `--no-resolve` guards → price → stock clamp → parse cargo → free-space clamp → parse finalQty. |
| R95 | `qty`, `unitPrice` and `finalQty` may reach the wire fractional. |
| R96 | **No signal handling at all.** `tokio`'s `signal` feature is not enabled, so a graceful-shutdown handler is a build failure, not a review question. |
| R97 | `EPIPE` and every other write error is swallowed; the exit code is untouched. `edm market Colonia \| head -5` exits cleanly. |
| R98 | `--requeue` has no backoff and is unbounded; a permanently-500ing market can loop indefinitely. |

### A.6 CORRECT — registered divergences

| # | Divergence | Justification |
|---|---|---|
| C1 | `ARDENT_MODULE` accepted and ignored; the four used exports compiled in; `EDM_ARDENT_BASE` replaces it. | Rust cannot `import()` TypeScript. `xtask ardent-contract` executes the real `ardent.ts` under Bun and diffs both URL builders and both parsers — turning a hidden runtime coupling into a checked one. |
| C2 | Transport failure texts (connect refused, DNS, TLS) come from a table pinned by running Bun against each failure mode. | No shared runtime; undici's strings are unreproducible and unstable. Anything not in the table renders our own text and is flagged by the harness. |
| C3 | HTTP reason phrases from `StatusCode::canonical_reason()`. | hyper's h1 client discards the wire phrase. `edm-mock` is constrained to canonical phrases so this cannot mask other diffs. |
| C4 | `uncompressedsize` and bodies capped at 256 MiB; over-large → a printed error via `try_reserve_exact`. | `vec![0; 9e15]` **aborts** the process; JS throws a catchable `RangeError`. Same outcome class, no abort. |
| C5 | UTF-8 decode failure message is ours, not Bun's engine-internal `TypeError`. | Reachable only on a corrupt body; the exact Bun string is recorded so the allowlist row is precise. |
| C6 | ChaCha20 refuses at 2³²−1 blocks; the TS at 2³². | 64 bytes of difference at 274 GB; the C4 cap fires first. Same error string. |
| C7 | The two dead key/nonce length errors cease to exist. | Both unreachable in the TS; encoded as `&[u8; 32]` and `Nonce([u8; 12])`. |
| C8 | A timed-out attempt is cancelled hard and prints nothing. | Proven equivalent for printing (the only `await` in between is `response.text()`, whose continuation is a microtask that runs first) and enforced by `game_api_failure_report_is_atomic`. Structured cancellation is strictly stronger, and the TS's late output is nondeterministic, which would break the byte-diff harness. |
| C9 | A first-attempt timeout still requeues and re-POSTs to EDDN (**preserved**), but `EDM_EDDN_ONCE=1` suppresses the second POST. | The double-post breaches EDDN's one-minute rule. Preserved by default, with a test asserting the bug so a fix must delete a test and add a register row. |
| C10 | On a fatal error we drop the worker pool; the TS leaves the other 15 running. | Only reachable via the `RangeError` C4 removes. Detached work after a fatal error is indefensible. |
| C11 | `COLUMNS` clamped to 10,000. | `COLUMNS=99999999999999999999` makes the TS attempt `"=".repeat(1e20)` at **module init**, outside `main`'s try/catch. |
| C12 | The envelope plaintext is not retained, only its byte length; the buffer is zeroized after sealing. | The TS reads only `.length` from it. Security gain, zero observable difference. |
| C13 | Non-ASCII collation falls back to scalar order; `--features icu-collation` is the escape hatch. | ICU-exact ordering costs ~1 MB for a corpus that is ASCII in practice, and bare `localeCompare()` is locale-dependent so the TS is not reproducible across environments anyway. |
| C14 | `POI_TYPE_LABELS["constructor"]` returns the raw string; the TS dies at `type.toUpperCase()`. | Reproducing a mid-table `TypeError` would require modelling a function value for zero benefit. (The *CLI* prototype hit, R47, **is** reproduced — it is cheap and changes the exit code.) |
| C15 | A lone `\uD800` escape or an out-of-range float fails to parse; the TS accepts both. | `serde_json`'s lexer rejects them; neither can occur in game-internal API data. Routed into the same `emitOpaquePayload` path so the shape degrades identically. |
| C16 | `writeFileSync` I/O error text is Rust's, not Node's. | Engine-internal. The *ordering* is preserved: the dump happens before `JSON.parse`, and `--json --dump f` writes nothing. |
| C17 | `edm --help constructor` prints the exact JSC message and exits 1, but no stack trace. | The TS throw is outside `main`'s try/catch so Bun adds an unhandled-rejection trace. Exit code and message match; the trace does not. |
| C18 | The two unreachable TS strings (lines 986, 1021) have no `ArgError` variant. | Proved unconstructible by the disjointness of `VALUE_FLAGS`/`BOOLEAN_FLAGS` plus the slot-type proptest. Recorded in `PORTING.md`; dead variants would be worse. |
| C19 | `User-Agent: edm/1.0.0` on Ardent, EDDN and Spansh. | Bun would send `Bun/x.y`; EDDN asks senders to identify themselves, and so should anything querying a third-party index. Frontier requests always carry their own per-request UA. |
| C20 | Wire header **order** and the `Accept-Encoding` value differ. | `HeaderMap` is hash-ordered; reqwest picks its own encoding list. Normalised in the wire diff; no server depends on either. |
| C21 | No 0–25 ms requeue latency floor; excess workers park in `recv()` instead of spinning 40 Hz timers. | This is the busy-wait removal. Observable only as line *ordering* when the queue empties while jobs are outstanding; those scenarios are compared as a multiset and must be justified in the scenario file. |
| C22 | `--timeout` above `INT32_MAX` clamps to 1 ms without Node's `TimeoutOverflowWarning`. | The clamp itself is preserved; the warning is `process.emitWarning`. |
| C23 | `--method connect/trace/track` and a non-ASCII `--user-agent` rejected with our message. | `fetch` forbids them before any socket; matching the *behaviour* matters more than the `TypeError` text. |
| C24 | `EDM_ORIGIN_OVERRIDE`, `EDM_ARDENT_BASE`, `EDM_EDDN_URL`, `EDM_SPANSH_BASE` added. | Harness plumbing. Unset, behaviour is byte-identical. |
| C25 | `edm route` exists. Bun answers `Unknown command "route"` and exits 2. | A new command, not a port of one. `KNOWN_COMMANDS` is untouched and `route` dispatches from a disjoint `EXTENDED_COMMANDS`, so R48's ordering is unaffected. Confined to argv beginning with `route`. |
| C26 | Route-only flag names resolve **only** when the command is `route` (a two-pass parse against `Table::Base` / `Table::Extended`). | Widening `Flag::resolve` globally would make `edm market Colonia --pad L` succeed where the TypeScript exits 2 — a fidelity regression on argv the harness never runs, and so one no scenario would catch. The `parity-isolation` gate proves the two tables agree over every committed scenario's argv. |
| C27 | `route` paces its requests, backs off on `Retry-After`, trips a breaker and bounds retries by wall clock. | R84 and R98 continue to describe `edm market` exactly. The original has no pacer at all: `game-internal-api.ts:2988` is a `// Request pacing` header with an empty body, and a 429 is requeued immediately with no delay and no header read — affordable for a seven-market system sweep and not for a thousand-market region. |
| C28 | `route --json` is one well-formed document. | R76's leak — a diagnostic in the middle of the stream — is faithful for the ported commands and is reproduced for them. `route` has no oracle to be faithful to, and a document a consumer cannot parse is not an output format. `Out::aside` sends the plan and diagnostics to stderr when stdout is a document. |
| C30 | `--deadline` bounds the loop search as well as the sweep and its retries. | The optimiser had no clock and no bound: a radius-100 loop search is tens of minutes of silence. It degrades to `Heuristic { SearchBudgetExhausted }` and never claims an optimum it did not prove. |
| C34 | **EDDN uploads are signed `edm`, not `int-market-sync`.** | The original's name, and the port said it too — so every row this program contributed to a shared dataset was attributed to a different piece of software. EDDN records the name so that consumers and maintainers can find a sender that misbehaves, and know when one is fixed; being byte-identical to the original is worth a great deal here, and it is not worth signing somebody else's name. `--software-name` still overrides it. The usage text's one line stating the default changes with it, and is the only line of the eighty-nine exempted from the byte pin — a help text documenting a default the program does not use is a worse thing for a reader than a masked line is for a test. The two EDDN parity scenarios pass `--software-name edm` to **both** sides, so the posted body and its `content-length` stay compared byte for byte. |
| C35 | **`edm vendor` exists.** Bun answers `Unknown command "vendor"` and exits 2. | A third command in the disjoint `EXTENDED_COMMANDS` set. It implements the observed read-only `GET /2.0/elite/vendors/items` envelope (`cmdrId`, station-scoped `marketId`, `vendorType=1`, credentials), uses Ardent to resolve targets and optionally enumerate a cap-aware `--radius`, and aggregates Pioneer Supplies stock without changing the four oracle-pinned commands. |
| C33 | **`edm eddn <kind>` exists.** Bun answers `Unknown command "eddn"` and exits 2. | A second command in the disjoint `EXTENDED_COMMANDS` set, dispatched by the same two-pass parse C26 describes, so `KNOWN_COMMANDS` and R48's ordering are untouched. It relays named markets to EDDN on purpose, where `route --eddn` publishes only what a route search happened to read — bounded by a radius and filtered to berths a large ship can use, which is the wrong shape for refreshing a region whose data has gone stale. |
| C32 | **EDDN quantities and prices are truncated to integers.** | The game-internal API sends fractional quantities — `Water` with a demand of `113.47560000000001` is a real row — and EDDN's schema types `demand`, `stock`, `meanPrice`, `buyPrice` and `sellPrice` as `integer`. Measured 2026-08-06 over 29,152 cached markets: 29,370 fractional values, and **29.7% of markets carry at least one**, so nearly a third of all uploads were answered `400 FAIL: Schema Validation`. Truncation rather than rounding, matching `EDMarketConnector/plugins/eddn.py:624-629` — EDDN is a shared dataset whose value depends on senders agreeing, and a rounding rule of our own would put this program's rows subtly out of step with every other uploader's for the same market. Brackets are **not** coerced: `levelType` is the enum `[0, 1, 2, 3, ""]` and no observed value falls outside it, so truncating one would turn an unexpected value into a plausible wrong one. |
| C31 | **`authToken` is checked against a floor of 512 characters, not the original's exact 2024.** | A live token measured 2026-08-06 is **2022** characters, so 2024 is one observation written down as a law — and it made the program refuse a credential the game itself was using, on every command. The check is kept because a half-pasted token is a real mistake with a confusing failure; the exact length is not, for a value this program does not issue. `machineToken` keeps its exact check at 80, which every observed value has matched. |
| C29 | `EDM_JITTER` pins backoff jitter to a fixed fraction of the window. | Backoff jitter is the one random quantity a recorded run cannot reproduce, and it decides how many attempts fit inside a wall-clock budget. It delegates `nonce_bytes` untouched: a pinned nonce is a separate decision with a separate flag. |
| C36 | **Superseded by C37, and kept because it is why C37 is shaped as it is.** `route --carrier-access` filtered fleet carriers on the docking access *Spansh* publishes. `--carriers` defaults it to `open`; without `--carriers` it is `any`, sends nothing, and the flag itself is refused. `open` drops the four restricted values and keeps carriers with none published; `proven` drops those too. | The defect: `--carriers` ranked carriers the commander cannot dock at, and a private carrier's prices are the *best* in a region precisely because nobody is arbitraging them — so it wins every hop and the whole answer is a flight to a door that will not open. Frontier *does* publish this — `2.0/elite/fleetcarrier/info` carries `docking.accessLevel` authoritatively — but at **one metered request per carrier**, out of the same budget and behind the same pacer as the market reads the route is for, and only after `fleetcarrier/system` has mapped the system to its carriers (that endpoint's own `docking` is always `null`). Measured: 199 carriers survived the ordinary filters on one real run, so the authoritative path is 199 requests spent on filtering rather than on prices, against Spansh's **two free requests per five hundred carriers**. Cost is the whole reason for the choice, and Spansh is a crowd-sourced cache of the same fact rather than a rival to it. Nothing closer to hand carries it: the market payload's top-level keys are exactly `allowsDumping`, `commodities`, `inventory`; Ardent's station record has thirty-five fields and no access; the `starsystem` document models none. EDDN's `commodity/3` schema carries `carrierDockingAccess`, renamed from the journal's `CarrierDockingAccess`, and Spansh is the index that keeps it where Ardent drops it — the two were measured ingesting the same EDDN message one second apart, so this is the same observation with one more column, not a second opinion. **Unknown is a first-class third state**, roughly a third of carriers, kept by `open` and counted on screen: dropping a station on a missing field would silently narrow the search on something unmeasured, the rule `select.rs` already states for a missing arrival distance. `Squadron`, `Friends` and `Squadron Friends` are all dropped by `open` because nothing this program can read — the journal included — knows the commander's squadron or friend list; the value is spelled `open`, and those are not. Two id-filtered POSTs per 500 candidates at concurrency 4, server-side filtered, applied before the spend gate so a refused door never costs a market read, and cached per market id for six hours because access is an owner-mutable setting only republished when somebody docks. Four response guards refuse rather than trust a reply, because Spansh answers HTTP 200 to each: a `size` over 500 comes back as 25 rows, a misspelled filter key is ignored and returns the whole unfiltered set (caught as a carrier reported both restricted and open), the summed counts, and any id outside the batch. **A Spansh failure refuses the run.** `ardent.rs` will not let an outage read as an empty region because that is indistinguishable from a sparse one; here an outage would read as a *fuller* answer — "no carrier is restricted" — the same lie pointed the other way, silently restoring the defect this exists to remove. **The commander's own journal overrides Spansh in both directions.** Spansh reported market 3712438528 (`1GOT`, Nessa) as `All`, having last heard from it on 2026-08-25 06:57Z; this commander's ship was answered `DockingDenied` / `Reason: "RestrictedAccess"` by that carrier at 2026-08-26 07:18:31Z and the route still recommended it as the top hop twelve times over. A crowd-sourced index cannot be fresher than its last reporter and cannot know which squadron the reader is in, so where the two disagree the one that was actually there wins. `Docked` at a `FleetCarrier` records `Admitted` and `DockingDenied`/`RestrictedAccess` records `Refused`, newest timestamp winning; no other denial reason is read, because `NoSpace` is a full pad, `Distance` is a bad approach and `TooLarge` is a fact about the ship, and none of the three is a statement about who the door opens for. The `Admitted` direction is the more valuable half: it is the only thing that can rescue a squadron- or friends-only carrier the commander is genuinely admitted to, which `open` would otherwise drop because nothing else this program reads knows their squadron or friend list. `CommanderState::carrier_doors` is therefore **exempt from `reset_for_load_game`**, alone among its fields: everything else there is a session value whose staleness would produce a confidently wrong route, and a door is not — a carrier that refused this commander last week is still refusing them after they quit to the main menu. | **Two claims in this row are now known to be false and are corrected rather than preserved, because they are facts about the API and not records of a decision:** Frontier *does* publish docking access, at `2.0/elite/fleetcarrier/info`; and the `fleetcarrier/system` prerequisite this row prices that path on does not exist, because `market_id = fleetCarrierId * 256 + 3_290_400_000` makes the id pure arithmetic. The rest stands as written: it was the correct decision on the information available, and the request-accounting problem C37 had to solve exists precisely *because* C36 chose a source that was free. |
| C37 | **`route --carrier-access open\|proven` reads docking access from Frontier, not Spansh.** One `2.0/elite/fleetcarrier/info` per candidate carrier, run as a priced phase with a gate of its own before the sweep is priced. | The defect C36 was written for, arriving by a different road. A crowd-sourced index is stale **by construction**: the only thing that republishes a carrier's access is somebody docking there and opening the market screen, so a door closed yesterday reads as open until the next visitor finds out — and the commander who finds out is the one who flew there. That happened. `docking.accessLevel` answers live and answers for a carrier the commander has never seen. Two live measurements make it affordable and neither was reasoned about: **`market_id = fleetCarrierId * 256 + 3_290_400_000`** (157/157 captured pairs, confirmed live on `3711014400` → `T1N-W2F` → `squadronfriends`), so the `fleetcarrier/system` round trip C36 priced does not exist and a market id that is not congruent is not a carrier's — the arithmetic is its own filter; and **the reply echoes both ids back**, so every answer is checkable against its question, which `market/list` cannot offer because its payload carries no id at all. What remains is one metered request per carrier — 199 on the one measured region — so the probes are a **phase**: a free cache drain and id guard, then a gate titled CARRIER ACCESS PLAN, then the requests through the run's own `Pacer` (which is what puts them in `spent.requests`, so the coverage arithmetic closes with nothing added back by hand), then the filter, then the sweep's gate with the probe count folded forward. Nothing is sent before its own phase is approved. `--dry-run` at an intermediate gate returns the new `Decision::Skipped` rather than `Stopped`, so it still prints the sweep plan behind it. **`notoriousAccess` is part of the verdict** when the journal's `Statistics.Crime.Notoriety` is non-zero: 11 of the 31 carriers Frontier calls `all` refuse a notorious commander, a 35% error rate that Spansh, EDDN and the journal's own `CarrierDockingAccess` all miss identically because none of them carries the field. It gets its own counter and its own exclusion row. `DockingDenied` now also accepts `Reason: "Offences"`, which is the same kind of statement as `RestrictedAccess` — about who is asking, not about the pad. The journal overlay narrows: `Admitted` always wins, because whether *this* commander is in the owner's squadron is the one question Frontier cannot answer; `Refused` beats a cached verdict and loses to one probed this run, which resolves on a fact the resolver already has and needs no date parser. Verdicts cache **raw** (level, notoriety flag, owner ids) under `frontier-carrier-access` for **15 minutes** — the derived verdict is never cached, so clearing notoriety takes effect on the next read rather than the next expiry; `--max-age` can tighten it and never extend it; and the `provider` field `bank` had always written is now actually *read*, so a Spansh-era entry cannot be served as a Frontier one. A carrier that no longer exists answers **HTTP 204 with an empty body** (measured, not assumed) and is `Restricted` under any reading, so the filter also removes decommissioned carriers Ardent still lists. A failed probe is `Unknown`, counted, named and **not banked**; the run ends only when *nothing* answered, which is what a dead credential looks like and where ranking two hundred unread carriers under `open` would hand back exactly the unfiltered list the user asked not to have. Spansh, `EDM_SPANSH_BASE`, `SPANSH_BASE_URL` and the harness's fourth profile are removed; the old `spansh-carrier-access` cache directory simply goes unread. |
| C38 | **`edm route` ranks from cached prices and then re-reads the markets behind the routes it is about to print, rescoring until the presented list is one it measured.** `--quick` reads the price cache at all for the first time; `--max-age` defaults to 24 hours instead of 30 minutes; fleet carriers are never served from cache at any age. | `--quick` polled every nominated market on every run — 2,073 markets and 84 s on one radius-500 run — and re-polled all of them on the next, because `quick.rs` handed `Cache::get` a refresh-mode cache and the `--max-age` it passed one line above was dead code. Only a handful of those markets are ever flown; the rest are polled to rank them. **Ranking from cache alone would have been worse, not merely cheaper.** Measured over the two cache generations on disk (2026-08-06 → 2026-08-27, median gap 20.8 days, 8,219 markets in both): choosing the best hop per commodity on the old prices and re-pricing on the new, **90.5% of picks were worse than predicted**, the median realised 53% of the promised spread, the aggregate 47.6%, and 8.6% were dead. That is winner's curse — a ranker selects extremes and extremes are disproportionately the stale-optimistic errors — and it **shrinks with a shorter lifetime but never disappears**, because the maximum of N noisy estimates is biased upward at any noise level. So the correction is not a tighter cache but a verification pass: re-read the markets behind the ranked routes, rescore, and repeat, because rescoring can demote a route and thereby promote one nobody has measured. Simulated over a 700-market region, a fully verified top-20 converged in **4 rounds having read 6.7% of the region**; `MAX_VERIFY_ROUNDS` is 8 and a run that hits it says so. **What verification cannot do is discover**: a route the first ranking buried is never found by rescoring, and widening the verified set tenfold recovered one route of twenty in simulation — so every rescored route carries `HeuristicReason::RescoredAfterSearch` and reads *"verified, best of these"* rather than an optimality claim. **Carriers are exempt from the cache entirely.** Over the same 21 days a carrier's *price* is no less stable than a station's (8.9% vs 11.3% moved more than a quarter), but a carrier row is 3× more likely to have stopped covering a full hold — 4.8% against 1.6% — because a carrier order is a fixed pot one commander set and it drains where a station's demand regenerates. Quantity decides whether a hop is real, so the one field cache cannot be trusted on is the one that matters. Ordering is load-bearing throughout: verification runs before the coverage block and before the per-commodity table, so *"every price below was read live during this run"* is decided after the live reads rather than before them — a run that ranks from cache and then verifies its winners reports the requests it actually sent. `TradeGraph::build` is also no longer run for a one-way search, which never reads it: 127 s and a 4.1 GiB peak at 5,049 markets, spent on nothing. The floor caveat moves with it, read from `limits.min_profit` rather than off the graph, because `single::solve` applies the same floor and a one-way run must not stop declaring a filter it is still performing. |
| C39 | **The ranking table drops `Cr/h` and gains `Stock/Demand`.** `--per-hour` puts the rate back; `Profit` is promoted to never-dropped in its place. A leg that buys from a fleet carrier carries a new caveat saying the stock does not restock. | `Cr/h` is the column that most often means nothing. It annualises a lap, and a lap is only repeatable if the seller's shelf refills — which a station's does and **a fleet carrier's does not**: that stock was put there by one commander, and once bought out it stays empty. Carriers are frequently the most profitable row in the table, so the number that reads as "a billion an hour" describes a flight that can be made exactly once. What replaces it is the pair that says where the profit came from and whether it will be there twice: what the seller has against what the buyer will take, for the leg that bound the route — `6,870/2,100` says the buyer's headroom bound this hop and the hold never came near it, which no other column in the table could tell you. The binding leg is the one with the smallest units, which for a single hop is the only leg; an unpublished destination quantity reads `?` rather than the full hold the optimiser assumed. Two consequences are handled rather than accepted. **The ordering key is now off screen**, and `Profit` over `Lap` does not reconstruct it — a single hop's rate is measured over the first lap including the approach, where `Lap` is the cycle — so the table states what it is ordered by in a note, because the profit column reading as unsorted is the exact defect the rate column was originally added to fix. **And `Profit` was priority three**, the first thing dropped, which was correct while the rate was the headline; it is priority zero now, since a ranking that fits a hundred columns by discarding the amount of money involved is not a ranking. The flag is `--per-hour` and deliberately **not** `--rate`: `rate` is already a base-table alias for `--concurrency`, route-only names resolve only after the base table has missed, so a flag spelled `rate` here would be dead code and a user typing it would silently set the worker count. Carrier detection needs no station type and none reaches the optimiser: a carrier's market id is `fleetCarrierId * 256 + 3_290_400_000` \[C37\], so congruence on that stride *is* the test. |
| C40 | **The ranking table gains `To start`: how far the ship is from the market a route begins at.** Measured from the commander's own position by default, or `--from <system>`, and `-` when neither is known. | The model has never included the approach. `Geometry::startup_millis` charges supercruise from the arrival star, docking and the market screen — it starts its clock in the source system — so the rate, the lap and every other column describe a route as if the ship were already parked there. That is correct for a *rate*, and it means a 65 Mcr hop next door and one three hundred light years away were previously indistinguishable in the table. The distance is not folded into the rate, because it is paid once and the rate is per lap; it is shown beside it so the reader can make that trade themselves. **`--from` is deliberately not the positional.** The positional says where to *search*, and searching a distant region does not move the ship: standing in Sol while ranking Mundii are two facts and the table needs both. Defaulting to the commander's own location costs nothing — the journal's `Location`/`FSDJump` already carries coordinates in `CommanderState`, so no request is made — and where the journal is silent the column reads `-` rather than substituting the search centre, which would print a confident `0.0` for a route the commander cannot reach today. |
| C41 | **`edm sell` exists.** Bun answers `Unknown command "sell"` and exits 2. It reads the hold from `Cargo.json`, finds who buys it, and plans the disposal — which markets, in what order, how much at each — ranked by **credits minus time**, not credits per hour. | A fourth command in the disjoint `EXTENDED_COMMANDS` set, and a separate command rather than a mode of `route` for the reason C33 gives about `eddn`: route's nomination is the wrong shape. `nominate_hops` pairs sellers against buyers and skips any row without a positive *buy* price, so a hold that has already been bought is invisible to it by construction; and `RouteConfig` would have to carry a `cargo` that means *free space* while this command means *contents*, the same word for the opposite quantity. **The objective is `W − λ·T` and this is the load-bearing decision.** Maximising credits alone always takes the `--stops` dearest buyers however far away they are. Maximising credits per hour is degenerate in the other direction and hides it: a disposal is finite, so the fastest route to a high rate is to sell part of the hold nearby and stop — 800 t in 19 minutes beats 1,232 t in 41 on rate while leaving 432 t aboard. `W − λ·T` is instead the literal form of the question a commander actually asks, *is the extra stop worth the flight*: take it exactly when `Δcredits > λ·Δtime`. It never rewards leaving cargo aboard, and unsold tonnage becomes a reported outcome instead of an exploit. λ defaults to the rate of the best single stop **that clears the hold** — deliberately not the best single stop by rate, which is usually a partial sale and would set the bar to something nothing can beat, collapsing the objective back onto the rate it exists to avoid. Setting λ to the incumbent's own rate and iterating is Dinkelbach and converges to that rate, which is why a rate mode is not offered: it is the fixed point of this one. `--worth` moves the bar and the alternatives table prints the **marginal** rate of each refused plan, so the decision is arithmetic the commander can move rather than a verdict they must argue with. That table **names the markets of every plan it lists**: a row reading "most credits, 2 stops, +30 Mcr" is arithmetic about something the reader cannot see, with no way to tell how the alternative differs from the recommendation or whether they would take it. **What is exact and what is not:** given the candidate set and the live prices, both the allocation and the ordering are provably optimal — allocation because a disposal has no purse, so the two-resource coupling that limits `greedy_fill` to `MultiCommodityFill` is simply absent, and within one commodity revenue is linear in tons so filling the dearest buyer first is the exchange argument; ordering because ≤ 4 stops are enumerated. The heuristic is the candidate set, trimmed by Ardent's 1,000-row cap, its 7-day silence window and `--top`, so every answer carries `NodesCapped` and never an optimality claim about the region. The search bound is `--stops` and it is visible because it *is* the bound: ordered paths are a falling factorial, capped at ten million, and the refusal names the count. `edm-route/src/sell.rs` joins the exactness gate's `SOLVING_PATH` and holds no floating point at all — the geometry moved to `time.rs`, which the gate exempts, behind `Geometry::millis_from`. **Stolen and mission cargo are excluded and named, never guessed at:** a stolen ton needs a black market even when the commodity is legal everywhere (`derive_black_market` is `stolen \|\| illegal`, and the two are independent), so a station answers HTTP 401 — and it cannot be nominated either, because Ardent publishes one open-market price per row, its `/markets` rows carry no `blackMarket` flag, and `RawCommodity` drops the `fencePrice` the live payload does carry. The exclusion is a gap in the *index*, not in what a market read can see — `Commodity::fence_price` is parsed and `edm market` prints it — and closing it would buy access rather than price: measured over 317,599 illegal rows in 9,000 cached reads, the fence pays exactly the open-market price in 66.2% and a median 0.02% more in the rest, never less. Bulk price decay is out of scope and the module says why: `Demand.bulk` is only ever populated under `--verify-systems`, it returns `None` for exactly the unpublished-demand rows that cluster among the best-priced buyers, and the integer function is **not concave** — measured, 63 marginal increases over a 1,232-ton range on this crate's own test vector — so the marginal greedy it would need would not be exact anyway. Two latent hazards were fixed in the same change because this command is the first to depend on them: `extended_command`'s tail had no `args.command == "route"` guard, so any name added to `EXTENDED_COMMANDS` without an arm silently ran **route** under that name; and `apply_trade_to_cargo`'s Sell branch decremented `count` without touching `stolen`, so selling the clean thousand out of a 1,000-clean/232-stolen stack left a stack the model believed was wholly stolen. Nothing read `stolen` until this command did. |
| C42 | **A carrier is dropped when Frontier says it is no longer in the system the index nominated it from**, and `edm sell` runs the docking-access phase at all. `/info` answers both questions in one reply, so the position check is free once access is being checked. | Ardent only learns a carrier has jumped when somebody reports it at its new home, and a carrier that has moved away from a quiet system may not be reported for days. Meanwhile **a carrier's market answers by `market_id` from anywhere in the galaxy** — the price and demand come back HTTP 200, fresh, entirely real, and say nothing whatever about where the ship would have to fly to trade on them. So live verification, which C38 added precisely to stop stale data reaching the reader, cannot catch this class at all: the reading it makes is not stale. The system is the one field a market read does not check. Measured on the run that exposed it: `VBV-WKK`, Ardent `Col 285 Sector MO-N b21-4` as of three days earlier, Frontier `systemAddress 3515170670963` (Faroahy, 170 Ly away) — nominated at `0.0 Ly` because the commander was standing in the system Ardent believed it to be in, and quoted a live 385,118 cr/t that was live and correct and unreachable. **`location.current.systemAddress` is compared against the `systemAddress` of the Ardent station row that nominated the market**, not against the search centre: the question is whether the row's premise still holds, and a mismatch invalidates the row whoever is asking. The removal is reported under its own label rather than folded into the access count, because "cannot dock" and "not there any more" are different facts and a commander who sees only the first will assume the rest of the list is positioned correctly. That same carrier was also `squadronfriends`, which `edm sell` never checked: the command accepted `--carriers` and `--carrier-access` and then ranked every carrier without probing one, so C37's filter was live in `route` and dead in `sell` — the phase now runs before the spend gate prices a single market read, so an undockable or departed carrier costs nothing to discard. Both probes and market reads are priced in one gate, against the pre-filter market count; the run therefore makes fewer requests than the ceiling was checked against, which is the safe direction for a ceiling. |
| C43 | **`edm route --quick --follow <s>` re-reads the ranking on an interval instead of printing once**, and `--follow-rounds` bounds it. Two latent defects had to be fixed first: retry backoff never slept, and the pacer could not survive a second round. | The request is "keep the top routes and their carriers fresh", and C38 already built the machinery: a verify round re-polls exactly the markets behind the ranked routes, patches them in place by index, and rescores. A follow round is that with the live set cleared, so the loop costs one sweep of the ranking rather than a search — the graph build is 127 s at five thousand markets, and a loop that re-solved would spend its whole interval searching. **Each round restores the original shortlist before rescoring.** `rescore` only ever filters and truncates, so without the restore a carrier offline for a single poll would be deleted permanently and could never return when it restocked; a long session would erode to nothing while looking like it was working. **`--max-requests` becomes a live ceiling.** It is otherwise checked only at the gate against an *estimate* and never against what was sent, so an indefinite loop would be bounded by nothing at all — this is the first place in the program that compares `pacer.spent().requests` against it. **The spelling is `--follow`, not `--watch`:** `Flag::Watch` is the ported boolean retry switch on `trade` and `market`, and `edm_route::watch` is the optimiser's cancellation handle, so the obvious name was taken twice over. Refused rather than ignored in two combinations: without `--quick`, because a full survey re-solves; and with `--json`, because C28 says route's document is one well-formed document or nothing and a loop emits one per round. The interval floor is 30 s — not a guess about what a commander wants but a limit on what this program will do to somebody else's server, and carrier verdicts cache for 15 minutes anyway. **Two defects fixed to make it safe.** `Pacer::started_ms` was a bare `f64` set once at construction and `tripped` was a first-trip-wins latch nothing ever cleared, so a session outliving `--deadline` would retire every later round's jobs instantly — rendering zero-polled coverage and flipping the exit code — and a single transient outage would silently poison every round after it. `begin_round` now reopens the deadline window and clears the latch, while deliberately keeping the adapted rate, the `Retry-After` hold-off and the cumulative spend, because those describe the server's behaviour and do not stop being true because a round ended. Separately, **`PinnedJitter::jitter_unit` returned `self.unit` unconditionally and every production call site passes `jitter.unwrap_or(f64::NAN)`** — `backoff_ms` multiplies by `unit.clamp(0, 1)`, NaN survives the clamp, and `js_max(delay, 0.0)` yields 0, so retry backoff never slept anywhere in the program and a failing job was requeued instantly eight times. The neighbouring test hid it by pinning `0.0`, a deliberate no-delay case rather than the production one. Unpinned now falls through to real entropy. That bug was harmless-ish in a one-shot run and would have been a retry storm in an indefinite one. |
| C30 | `--deadline` bounds the loop search as well as the sweep, and a search it stops reports `Heuristic { SearchBudgetExhausted }` rather than an optimum. | The search is part of the run, so "how long this may take" already covers it; a second wall-clock flag would let two limits contradict each other and would leave the default answer to "how long may the search take" at *forever*. Measured 2026-08-06: 78 s to build the graph and 205 s per improving Dinkelbach round at 5,000 markets. `edm-route` is pure and has no clock, so the budget arrives as a predicate the caller answers — never as a step count, which does not convert to seconds at any fixed rate. |

**Opt-in fixes, all off by default, each an allowlist row only when set:**

| env var | effect |
|---|---|
| `EDM_WIDTH=display` | `unicode-width` cell metric instead of UTF-16 code units — fixes CJK/emoji carrier-name misalignment. |
| `EDM_STRICT_JSON=1` | Routes R76's diagnostics to stderr so `--json` is parseable. |
| `EDM_EDDN_ONCE=1` | Suppresses a second EDDN POST after a late-timeout requeue (C9). |
| `EDM_EDDN_BRACKET=passthrough` | Emits `stockBracket: ""` instead of coercing to `0`, per the schema's own `levelType` semantics. |

**TS defects preserved, not fixed** (the port matches exactly; listed so they aren't mistaken for
port bugs): `statusFlags` never sent although the EDDN README says SHOULD for game-internal API sources;
`horizons` never inferred. (A fractional `stock` or `demand` forwarded verbatim *was* preserved here and reached a 400 on 29.7% of markets; it is now fixed under C32.)
