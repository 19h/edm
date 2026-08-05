<!-- The contract between market-request.ts and this port. -->

`market-request.ts` is the specification. Every observable byte on stdout and
stderr, every exit code, and every byte on the wire is reproduced unless it
appears below marked **CORRECT**. Anything that differs and is not listed here
is a bug.

Rows are referenced from code comments and test names by their `R`/`C` number,
so renumbering them is a breaking change to the test suite.

## Status

| Step | Module | State |
|---|---|---|
| 1 | `edm_core::js` | done — 7 oracle fixtures green against bun 1.2.3 |
| 2 | `edm_core::js::json` | |
| 3 | `edm_core::wire` | |
| 4 | `edm_core::render` | |
| 5 | `edm_core::cli` | |
| 6 | `edm_core::domain` | |
| 7-10 | `edm` I/O layer | |
| 11-12 | sweep and commands | |
| 13 | `cargo xtask parity` | |

## Measured deviations from the design

Recorded when an assumption made while planning turned out to be wrong.

- **C13 was too weak.** The plan proposed an ASCII-only collation model with
  non-ASCII falling back to scalar order. Measurement killed it: `ß` expands to
  `ss`, `þ` sorts after `z`, `ø` is a secondary variant of `o`, `æ` groups with
  `a`, and `½` falls between `1` and `2` — and Elite Dangerous names contain all
  of these. `edm_core::js::collate` now delegates to ICU4X (the same CLDR data
  JavaScript engines use) and is pinned by a 7,279-row corpus. The
  `icu-collation` cargo feature is therefore unnecessary and does not exist.
- **`EDM_WIDTH=display` is a runtime `Metric`, not a cargo feature.** Cheaper to
  test and it keeps one binary.

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
| R67 | Redirects: CAPI **none**; Ardent and EDDN **follow, limit 20** (not reqwest's default 10). |
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
| C8 | A timed-out attempt is cancelled hard and prints nothing. | Proven equivalent for printing (the only `await` in between is `response.text()`, whose continuation is a microtask that runs first) and enforced by `capi_failure_report_is_atomic`. Structured cancellation is strictly stronger, and the TS's late output is nondeterministic, which would break the byte-diff harness. |
| C9 | A first-attempt timeout still requeues and re-POSTs to EDDN (**preserved**), but `EDM_EDDN_ONCE=1` suppresses the second POST. | The double-post breaches EDDN's one-minute rule. Preserved by default, with a test asserting the bug so a fix must delete a test and add a register row. |
| C10 | On a fatal error we drop the worker pool; the TS leaves the other 15 running. | Only reachable via the `RangeError` C4 removes. Detached work after a fatal error is indefensible. |
| C11 | `COLUMNS` clamped to 10,000. | `COLUMNS=99999999999999999999` makes the TS attempt `"=".repeat(1e20)` at **module init**, outside `main`'s try/catch. |
| C12 | The envelope plaintext is not retained, only its byte length; the buffer is zeroized after sealing. | The TS reads only `.length` from it. Security gain, zero observable difference. |
| C13 | Non-ASCII collation falls back to scalar order; `--features icu-collation` is the escape hatch. | ICU-exact ordering costs ~1 MB for a corpus that is ASCII in practice, and bare `localeCompare()` is locale-dependent so the TS is not reproducible across environments anyway. |
| C14 | `POI_TYPE_LABELS["constructor"]` returns the raw string; the TS dies at `type.toUpperCase()`. | Reproducing a mid-table `TypeError` would require modelling a function value for zero benefit. (The *CLI* prototype hit, R47, **is** reproduced — it is cheap and changes the exit code.) |
| C15 | A lone `\uD800` escape or an out-of-range float fails to parse; the TS accepts both. | `serde_json`'s lexer rejects them; neither can occur in CAPI data. Routed into the same `emitOpaquePayload` path so the shape degrades identically. |
| C16 | `writeFileSync` I/O error text is Rust's, not Node's. | Engine-internal. The *ordering* is preserved: the dump happens before `JSON.parse`, and `--json --dump f` writes nothing. |
| C17 | `edm --help constructor` prints the exact JSC message and exits 1, but no stack trace. | The TS throw is outside `main`'s try/catch so Bun adds an unhandled-rejection trace. Exit code and message match; the trace does not. |
| C18 | The two unreachable TS strings (lines 986, 1021) have no `ArgError` variant. | Proved unconstructible by the disjointness of `VALUE_FLAGS`/`BOOLEAN_FLAGS` plus the slot-type proptest. Recorded in `PORTING.md`; dead variants would be worse. |
| C19 | `User-Agent: edm/1.0.0` on Ardent and EDDN. | Bun would send `Bun/x.y`; EDDN asks senders to identify themselves. Frontier requests always carry their own per-request UA. |
| C20 | Wire header **order** and the `Accept-Encoding` value differ. | `HeaderMap` is hash-ordered; reqwest picks its own encoding list. Normalised in the wire diff; no server depends on either. |
| C21 | No 0–25 ms requeue latency floor; excess workers park in `recv()` instead of spinning 40 Hz timers. | This is the busy-wait removal. Observable only as line *ordering* when the queue empties while jobs are outstanding; those scenarios are compared as a multiset and must be justified in the scenario file. |
| C22 | `--timeout` above `INT32_MAX` clamps to 1 ms without Node's `TimeoutOverflowWarning`. | The clamp itself is preserved; the warning is `process.emitWarning`. |
| C23 | `--method connect/trace/track` and a non-ASCII `--user-agent` rejected with our message. | `fetch` forbids them before any socket; matching the *behaviour* matters more than the `TypeError` text. |
| C24 | `EDM_ORIGIN_OVERRIDE`, `EDM_ARDENT_BASE`, `EDM_EDDN_URL` added. | Harness plumbing. Unset, behaviour is byte-identical. |

**Opt-in fixes, all off by default, each an allowlist row only when set:**

| env var | effect |
|---|---|
| `EDM_WIDTH=display` | `unicode-width` cell metric instead of UTF-16 code units — fixes CJK/emoji carrier-name misalignment. |
| `EDM_STRICT_JSON=1` | Routes R76's diagnostics to stderr so `--json` is parseable. |
| `EDM_EDDN_ONCE=1` | Suppresses a second EDDN POST after a late-timeout requeue (C9). |
| `EDM_EDDN_BRACKET=passthrough` | Emits `stockBracket: ""` instead of coercing to `0`, per the schema's own `levelType` semantics. |

**TS defects preserved, not fixed** (the port matches exactly; listed so they aren't mistaken for
port bugs): `statusFlags` never sent although the EDDN README says SHOULD for CAPI sources;
`horizons` never inferred; a CAPI bracket of `4` or a fractional `stock` forwarded verbatim to a 400.
