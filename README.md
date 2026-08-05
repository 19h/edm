# edm

A client for Elite Dangerous' Companion API: poll a market, sweep every market
in a system, execute trades, and publish commodity data to
[EDDN](https://github.com/EDCD/EDDN).

```
edm market --market-id 4306502403        # one market's commodity listing
edm market Colonia --eddn                # sweep a system, publish each market
edm markets "Hyades Sector NI-X a16-0"   # what is dockable in a system
edm trade --market-id 4306502403 --type buy --item silver --qty 10
edm trade --type buy --item palladium,gold --cargo 1232 --fill --watch
edm help
```

## What this is

A Rust port of `market-request.ts`, which is still in this repository and is not
going anywhere. It is the specification, the oracle, and the tie-breaker: when a
question comes up about what the program should do, the answer is whatever that
file does when you run it.

The port is not a rewrite in spirit. Every observable byte — stdout, stderr, the
exit code, the bytes on the wire — is reproduced, and the places where it
deliberately is not are enumerated in [`PORTING.md`](PORTING.md) with a reason
each. That register is the contract; anything that differs and is not listed
there is a bug.

## Layout

```
crates/edm-core/    pure: no I/O, no clock, no entropy, no network
  js/               ECMAScript semantics Rust does not share
  wire/             ChaCha20, base64, raw LZ4 blocks
  render/           the fitted table, and every view built from it
  cli/              the command-line grammar and per-command configuration
  domain/           the market model, ID64 codec, EDDN messages, trade rules
crates/edm/         everything impure, and the command entry points
xtask/              fixture generators, the mock server, the parity harness
```

`edm-core`'s purity is checked rather than asserted: `cargo xtask gates` fails
the build if `tokio`, `reqwest`, `rustix` or `getrandom` ever appear in its
dependency tree. The point is that every behaviour in the parity register stays
reachable from a plain `#[test]`, with no runtime and no sockets.

## Why there is a `js` module

The program's observable behaviour is inherited from JavaScript semantics that
Rust does not share, and the differences are not cosmetic:

- **`Number::toString`** — `1e21` prints as `1e+21`, not as twenty-one zeros;
  `-0` prints as `0`; an integral double prints without a decimal point. That
  last one is not a nicety: EDDN validates its schema with CPython, where
  `isinstance(123.0, int)` is `False`, so a payload serialized by `serde_json`
  would be rejected with HTTP 400 on **every** upload — and the EDDN
  specification forbids retrying that.
- **Object key order** — `Object.entries()` hoists canonical array-index keys
  into ascending numeric order ahead of everything else. Commodity ids are
  indices; market ids are past the 2³²−2 limit and are not. So one map is
  silently re-sorted and the other keeps document order, and that ordering
  reaches the sweep queue, the progress lines and the EDDN commodity array.
- **String length** — the renderer measures in UTF-16 code units, because that
  is what `String.prototype.length` counts.
- **Collation** — every table sorts with `localeCompare`. Byte order would put
  every uppercase letter before every lowercase one.

These are concentrated in `edm_core::js` and pinned by fixtures generated from
the same Bun build that runs the original. Measuring an engine beats reasoning
about one: the first run of those fixtures found that `toFixed(1)` was keeping a
negative zero the specification discards, and that a hand-written CLDR collation
model was wrong in six distinct ways.

## Verifying it

```bash
cargo test --workspace       # units, property tests, snapshots, oracle fixtures
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask gates            # purity, no-signal, secret scan
cargo xtask parity           # the acceptance gate: byte-diff against Bun
```

`cargo xtask parity` is the definition of done. It runs the same argv through
the TypeScript original and the Rust binary against the same mock Frontier,
Ardent and EDDN server, and diffs stdout, stderr, the exit code, any `--dump`
file, and the recorded wire log. Determinism comes from the program's own flags:
`--nonce`, `--f-time` and `--request-time` pin the request stamp, so the
encrypted query is byte-identical on both sides.

`cargo xtask bless` regenerates the oracle fixtures. Review that diff carefully —
each fixture records the `bun --version` that produced it, and a change there
means the reference behaviour moved.

## Credentials

Four values, by flag or environment:

| flag | environment | notes |
|---|---|---|
| `--cmdr-id` | `COMMANDER_ID` | |
| `--machine-id` | `MACHINE_ID` | |
| `--machine-token` | `MACHINE_TOKEN` | exactly 80 characters |
| `--auth-token` | `AUTH_TOKEN` | exactly 2024 characters |

The two tokens are held in a `Secret`, which has no `Display`, no `Serialize`,
no `Deref` and no `Clone`; its `Debug` prints a length; and its buffer is zeroed
on drop. The process disables core dumps and clears its dumpable flag before it
reads any of them.

`edm` runs with real credentials against a live account. Start with `--dry-run`.
