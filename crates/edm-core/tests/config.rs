//! Vectors for the per-command configuration builders.
//!
//! Two things are under test and only one of them is the returned struct. The
//! other is *when* each option is read, which is observable because a read can
//! throw and because network calls sit between some of the reads \[R50\]. Every
//! case that exists to pin an order says so in its name, and the ones that look
//! like bugs (a bad `--cargo` reported only after a stock clamp; `--item
//! "gold,"` taking the single path and then looking up a commodity called
//! `gold,`) are pinned deliberately.
//!
//! Expected message strings are transcriptions of `market-request.ts`, not
//! guesses; each is cited by line number where it is produced in
//! `cli::config`.

use edm_core::cli::config::{
    self, BatchConfig, CachedTimestamp, LookupMode, MarketTarget, MarketsConfig, PlanSource,
    ResolvedTrade, SessionConfig, StampDefaults, SweepSettings, TradeDispatch, TradeInputs,
};
use edm_core::cli::{Cli, CliError, EnvSnapshot, Table, parse, parse_with};
use edm_core::domain::trade::Kind;
use edm_core::domain::{MarketSnapshot, parse_market_snapshot};
use edm_core::js::json::JsValue;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Binds argv and an environment and hands over a [`Cli`] borrowing both.
fn with_cli<T>(argv: &[&str], env: &[(&str, &str)], body: impl FnOnce(&Cli<'_>) -> T) -> T {
    let tokens: Vec<String> = argv.iter().map(|token| (*token).to_owned()).collect();
    let parsed = parse(&tokens).expect("the vector table only contains argv that parses");
    let env = EnvSnapshot::from_pairs(env.iter().copied());
    body(&Cli::new(&parsed, &env))
}

/// What a case asserts: either the exact thrown message, or facts about the
/// value that came back.
enum Expect<T> {
    Err(&'static str),
    Ok(fn(&T)),
}

struct Case<T: 'static> {
    name: &'static str,
    argv: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    expect: Expect<T>,
}

/// Runs one table against one builder.
fn run<T: 'static>(
    cases: &[Case<T>],
    build: impl Fn(&Cli<'_>) -> Result<T, String>,
) {
    for case in cases {
        let outcome = with_cli(case.argv, case.env, &build);
        match (&case.expect, outcome) {
            (Expect::Err(expected), Err(actual)) => {
                assert_eq!(&actual, expected, "{}", case.name);
            }
            (Expect::Err(expected), Ok(_)) => {
                panic!("{}: expected error {expected:?}, got a value", case.name);
            }
            (Expect::Ok(_), Err(actual)) => {
                panic!("{}: expected a value, got error {actual:?}", case.name);
            }
            (Expect::Ok(check), Ok(value)) => check(&value),
        }
    }
}

/// A credential set that passes every validator, so that a case about some
/// *other* option is not shadowed by `loadCredentials` \[R50\].
fn good_credentials() -> Vec<(String, String)> {
    vec![
        ("COMMANDER_ID".to_owned(), "F1234567".to_owned()),
        ("MACHINE_ID".to_owned(), "abc".to_owned()),
        ("MACHINE_TOKEN".to_owned(), "m".repeat(80)),
        ("AUTH_TOKEN".to_owned(), "a".repeat(2024)),
    ]
}

fn session(argv: &[&str], extra: &[(&str, &str)]) -> Result<SessionConfig, String> {
    let mut env = good_credentials();
    for (name, value) in extra {
        // First-wins, so an override has to precede the default.
        env.insert(0, ((*name).to_owned(), (*value).to_owned()));
    }
    let tokens: Vec<String> = argv.iter().map(|token| (*token).to_owned()).collect();
    let parsed = parse(&tokens).expect("argv must parse");
    let env = EnvSnapshot::from_pairs(env);
    config::open_session(&Cli::new(&parsed, &env)).map_err(|error| error.message().to_owned())
}

// ---------------------------------------------------------------------------
// openSession / loadCredentials
// ---------------------------------------------------------------------------

#[test]
fn credentials_are_validated_field_by_field_in_source_order() {
    // ts:995 — the environment-aware spelling of the missing-option message.
    assert_eq!(
        session(&["market"], &[("COMMANDER_ID", " ")]).unwrap_err(),
        "Missing --cmdr-id (or COMMANDER_ID in the environment)",
        "R56: a blank environment variable is absent, not empty"
    );
    assert_eq!(
        session(&["market", "--cmdr-id", "F\u{e9}"], &[]).unwrap_err(),
        "cmdrId must contain printable ASCII only"
    );
    // machineId is validated before machineToken, so a bad machine id wins even
    // when the token is also wrong.
    assert_eq!(
        session(&["market", "--machine-id", "caf\u{e9}", "--machine-token", "short"], &[])
            .unwrap_err(),
        "machineId must contain printable ASCII only"
    );
    // Within a token field, ASCII is tested before length: this value is 80
    // characters long and still reports the character-set failure.
    let accented = "\u{e9}".repeat(80);
    assert_eq!(
        session(&["market", "--machine-token", &accented], &[]).unwrap_err(),
        "machineToken must contain printable ASCII only"
    );
    assert_eq!(
        session(&["market", "--machine-token", "abc"], &[]).unwrap_err(),
        "machineToken must be exactly 80 characters; received 3"
    );
    assert_eq!(
        session(&["market", "--auth-token", "abc"], &[]).unwrap_err(),
        "authToken must be exactly 2024 characters; received 3"
    );
}

/// A surrogate pair is two UTF-16 code units, and the length message counts
/// units \[R22\].
#[test]
fn the_length_message_counts_utf16_units() {
    let value = "\u{1f680}".repeat(40);
    assert_eq!(
        session(&["market", "--machine-token", &value], &[]).unwrap_err(),
        // The ASCII test fires first; what this pins is that it fires at all
        // for astral characters rather than being bypassed by a byte count.
        "machineToken must contain printable ASCII only"
    );
    let value = "\u{7f}".repeat(3);
    assert_eq!(
        session(&["market", "--machine-token", &value], &[]).unwrap_err(),
        "machineToken must contain printable ASCII only",
        "DEL is outside \\x20-\\x7e"
    );
}

#[test]
fn the_session_reads_credentials_before_its_switches() {
    // `--json` is poisoned, which would throw in `optionalSwitch` — but the
    // credentials are loaded first, so the credential failure is what surfaces.
    assert_eq!(
        session(&["market", "--json", "constructor", "--cmdr-id", "\u{e9}"], &[]).unwrap_err(),
        "cmdrId must contain printable ASCII only"
    );
}

#[test]
fn session_switches_and_method_override() {
    let good = session(&["market"], &[]).unwrap();
    assert_eq!(good.method_override, None);
    assert!(!good.dry_run && !good.full_url && !good.json);
    assert_eq!(good.credentials.commander_id, "F1234567");
    assert_eq!(good.credentials.machine_token.len(), 80);

    let overridden =
        session(&["market", "--method", "patch", "--dry-run", "--json"], &[]).unwrap();
    assert_eq!(overridden.method_override.as_deref(), Some("PATCH"));
    assert!(overridden.dry_run);
    assert!(overridden.json);
    assert!(!overridden.full_url);

    // Full-Unicode uppercasing, as `toUpperCase` performs \[R32\].
    let sharp_s = session(&["market", "--method", "stra\u{df}e"], &[]).unwrap();
    assert_eq!(sharp_s.method_override.as_deref(), Some("STRASSE"));
}

// ---------------------------------------------------------------------------
// nextStamp
// ---------------------------------------------------------------------------

const DEFAULTS: StampDefaults =
    StampDefaults { entropy: [0x0a, 0xbc, 0xde, 0xf0, 0x12, 0x34], now_ms: 1_700_000_000_500.0, uptime_seconds: 12.3456 };

fn stamp(argv: &[&str], env: &[(&str, &str)]) -> Result<config::RequestStamp, String> {
    with_cli(argv, env, |cli| {
        config::next_stamp(cli, DEFAULTS).map_err(|error| error.message().to_owned())
    })
}

#[test]
fn a_stamp_falls_back_to_its_ambient_defaults() {
    let value = stamp(&["market"], &[]).unwrap();
    assert_eq!(value.nonce.as_str(), "0abcdef01234");
    assert_eq!(value.frontier_time, 1_700_000_000.0, "floor(Date.now() / 1000)");
    assert_eq!(value.request_time, 12_345, "floor(uptime() * 1000)");
}

#[test]
fn stamp_overrides_come_from_flags_or_the_environment() {
    let value = stamp(
        &["market", "--nonce", "ABCDEF012345", "--f-time", "42"],
        &[("REQUEST_TIME", "7")],
    )
    .unwrap();
    assert_eq!(value.nonce.as_str(), "abcdef012345", "the request nonce is lowercased [R57]");
    assert_eq!(value.frontier_time, 42.0);
    assert_eq!(value.request_time, 7);

    let from_env = stamp(&["market"], &[("NONCE", "AAAAAAAAAAAA"), ("F_TIME", "9")]).unwrap();
    assert_eq!(from_env.nonce.as_str(), "aaaaaaaaaaaa");
    assert_eq!(from_env.frontier_time, 9.0);
}

/// `>>> 0` wraps rather than saturating \[R15\].
#[test]
fn request_time_wraps_modulo_two_to_the_thirty_two() {
    assert_eq!(stamp(&["market", "--request-time", "4294967296"], &[]).unwrap().request_time, 0);
    assert_eq!(stamp(&["market", "--request-time", "4294967295"], &[]).unwrap().request_time, u32::MAX);
    assert_eq!(stamp(&["market", "--request-time", "4294967297"], &[]).unwrap().request_time, 1);
}

#[test]
fn stamp_fields_are_validated_in_object_literal_order() {
    // All three are wrong; the nonce is reported because it is the first
    // property of the returned literal, not because it was read first.
    assert_eq!(
        stamp(&["market", "--nonce", "zz", "--f-time", "x", "--request-time", "y"], &[]).unwrap_err(),
        "nonce must be exactly 12 hexadecimal characters"
    );
    assert_eq!(
        stamp(&["market", "--f-time", "x", "--request-time", "y"], &[]).unwrap_err(),
        "fTime must be an unsigned decimal integer"
    );
    assert_eq!(
        stamp(&["market", "--request-time", "y"], &[]).unwrap_err(),
        "requestTime must be an unsigned decimal integer"
    );
    assert_eq!(
        stamp(&["market", "--f-time", &"9".repeat(100)], &[]).unwrap_err(),
        "fTime is outside the safe integer range",
        "R11: a 100-digit string passes the pattern and fails the range test"
    );
}

// ---------------------------------------------------------------------------
// runMarket
// ---------------------------------------------------------------------------

const MARKET_TARGET_CASES: &[Case<MarketTarget>] = &[
    Case {
        name: "R52 an explicit --market-id pins one market",
        argv: &["market", "--market-id", "0004306502403"],
        env: &[],
        expect: Expect::Ok(|target| {
            assert_eq!(*target, MarketTarget::Single(4_306_502_403.0), "leading zeros are parsed away");
        }),
    },
    Case {
        name: "R52 a name sweeps even when MARKET_ID is set",
        argv: &["market", "Colonia"],
        env: &[("MARKET_ID", "128666762")],
        expect: Expect::Ok(|target| assert_eq!(*target, MarketTarget::Sweep("Colonia".to_owned()))),
    },
    Case {
        name: "R52 market prefers --system over --station",
        argv: &["market", "--system", "Sol", "--station", "Abraham Lincoln", "Colonia"],
        env: &[],
        expect: Expect::Ok(|target| assert_eq!(*target, MarketTarget::Sweep("Sol".to_owned()))),
    },
    Case {
        name: "R52 --station is consulted only when --system is absent",
        argv: &["market", "--station", "Abraham Lincoln", "Colonia"],
        env: &[],
        expect: Expect::Ok(|target| {
            assert_eq!(*target, MarketTarget::Sweep("Abraham Lincoln".to_owned()));
        }),
    },
    Case {
        name: "positionals join on a single space and are js-trimmed",
        argv: &["market", "  Hyades", "Sector", "DB-X", "d1-112  "],
        env: &[],
        expect: Expect::Ok(|target| {
            assert_eq!(*target, MarketTarget::Sweep("Hyades Sector DB-X d1-112".to_owned()));
        }),
    },
    Case {
        name: "a whitespace-only positional is not a name",
        argv: &["market", "   "],
        env: &[("MARKET_ID", "3223343616")],
        expect: Expect::Ok(|target| assert_eq!(*target, MarketTarget::Single(3_223_343_616.0))),
    },
    Case {
        name: "R52 the environment is the last resort",
        argv: &["market"],
        env: &[("MARKET_ID", "3223343616")],
        expect: Expect::Ok(|target| assert_eq!(*target, MarketTarget::Single(3_223_343_616.0))),
    },
    Case {
        name: "ts:1699 nothing to go on",
        argv: &["market"],
        env: &[],
        expect: Expect::Err(
            "market needs a system name, or --market-id <id> (or MARKET_ID in the environment)",
        ),
    },
    Case {
        name: "R46 the flag error names the canonical spelling",
        argv: &["market", "--market", "abc"],
        env: &[],
        expect: Expect::Err("--market-id must be an unsigned decimal integer"),
    },
    Case {
        name: "the environment error names the variable, not the flag",
        argv: &["market"],
        env: &[("MARKET_ID", "abc")],
        expect: Expect::Err("MARKET_ID must be an unsigned decimal integer"),
    },
];

#[test]
fn market_target_cases() {
    run(MARKET_TARGET_CASES, |cli| {
        config::market_target(cli).map_err(|error| error.message().to_owned())
    });
}

// ---------------------------------------------------------------------------
// runMarketSweep
// ---------------------------------------------------------------------------

const SWEEP_CASES: &[Case<SweepSettings>] = &[
    Case {
        name: "sweep defaults",
        argv: &["market", "Colonia"],
        env: &[],
        expect: Expect::Ok(|settings| {
            assert_eq!(settings.workers, 5);
            assert_eq!(settings.timeout_ms, 10_000.0);
            assert_eq!(settings.requeues, 3.0);
            assert!(!settings.detail);
        }),
    },
    Case {
        name: "R51 --concurrency 0 clamps up to one worker",
        argv: &["market", "--concurrency", "0"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.workers, 1)),
    },
    Case {
        name: "R51 --concurrency clamps down to MAX_CONCURRENCY",
        argv: &["market", "--concurrency", "99"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.workers, 16)),
    },
    Case {
        name: "R51 --timeout is unbounded and fractional",
        argv: &["market", "--timeout", "0.0005"],
        env: &[],
        expect: Expect::Ok(|settings| {
            assert_eq!(settings.timeout_ms, 1.0, "Math.round(0.0005 * 1000)");
        }),
    },
    Case {
        name: "R51 --requeue is unbounded",
        argv: &["market", "--requeue", "1000000000"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.requeues, 1_000_000_000.0)),
    },
    Case {
        name: "R12 the timeout rounds half toward +Infinity",
        argv: &["market", "--timeout", "1.0005"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.timeout_ms, 1001.0)),
    },
    Case {
        name: "--detail",
        argv: &["market", "--detail"],
        env: &[],
        expect: Expect::Ok(|settings| assert!(settings.detail)),
    },
    Case {
        name: "R10 --timeout goes through Number(), which rejects `inf`",
        argv: &["market", "--timeout", "inf"],
        env: &[],
        expect: Expect::Err("--timeout must be a positive number"),
    },
    Case {
        name: "--concurrency is an unsigned integer, not a decimal",
        argv: &["market", "--concurrency", "2.5"],
        env: &[],
        expect: Expect::Err("--concurrency must be an unsigned decimal integer"),
    },
];

#[test]
fn sweep_settings_cases() {
    run(SWEEP_CASES, |cli| {
        config::sweep_settings(cli, false).map_err(|error| error.message().to_owned())
    });
}

#[test]
fn the_sweep_quiet_flag_is_the_sessions_json_flag() {
    let quiet = with_cli(&["market"], &[], |cli| config::sweep_settings(cli, true).unwrap());
    assert!(quiet.quiet);
}

#[test]
fn the_sweep_lookup_mode_asks_only_about_station() {
    let station =
        with_cli(&["market", "--station", "Jameson Memorial"], &[], config::sweep_lookup_mode);
    assert_eq!(station, LookupMode::Station);
    // `--system` does *not* produce `system` here, unlike `markets`.
    let system = with_cli(&["market", "--system", "Shinrarta Dezhra"], &[], config::sweep_lookup_mode);
    assert_eq!(system, LookupMode::Auto);
}

/// `||` short-circuits, so a poisoned `--eddn-test` is never read once `--eddn`
/// is on \[R47\].
#[test]
fn wants_eddn_short_circuits() {
    assert!(with_cli(&["market", "--eddn"], &[], |cli| config::wants_eddn(cli).unwrap()));
    assert!(with_cli(&["market", "--eddn-test"], &[], |cli| config::wants_eddn(cli).unwrap()));
    assert!(!with_cli(&["market"], &[], |cli| config::wants_eddn(cli).unwrap()));
    assert!(with_cli(&["market", "--eddn", "--eddn-test", "constructor"], &[], |cli| {
        config::wants_eddn(cli).unwrap()
    }));
    assert!(with_cli(&["market", "--eddn-test", "constructor"], &[], |cli| {
        config::wants_eddn(cli).is_err()
    }));
}

// ---------------------------------------------------------------------------
// runMarkets
// ---------------------------------------------------------------------------

const MARKETS_CASES: &[Case<MarketsConfig>] = &[
    Case {
        name: "R52 markets prefers --station over --system",
        argv: &["markets", "--system", "Sol", "--station", "Abraham Lincoln", "Colonia"],
        env: &[],
        expect: Expect::Ok(|config| {
            assert_eq!(
                *config,
                MarketsConfig::Lookup {
                    name: "Abraham Lincoln".to_owned(),
                    mode: LookupMode::Station
                }
            );
        }),
    },
    Case {
        name: "R52 --system resolves as a system, not as auto",
        argv: &["markets", "--system", "Sol", "Colonia"],
        env: &[],
        expect: Expect::Ok(|config| {
            assert_eq!(
                *config,
                MarketsConfig::Lookup { name: "Sol".to_owned(), mode: LookupMode::System }
            );
        }),
    },
    Case {
        name: "a positional resolves as auto",
        argv: &["markets", "Colonia"],
        env: &[],
        expect: Expect::Ok(|config| {
            assert_eq!(
                *config,
                MarketsConfig::Lookup { name: "Colonia".to_owned(), mode: LookupMode::Auto }
            );
        }),
    },
    Case {
        name: "--address wins outright and no name is resolved",
        argv: &["markets", "--address", "5378909424384", "--station", "Anywhere"],
        env: &[],
        expect: Expect::Ok(|config| {
            assert_eq!(*config, MarketsConfig::Address(5_378_909_424_384.0));
        }),
    },
    Case {
        name: "ts:3011 nothing to go on",
        argv: &["markets"],
        env: &[],
        expect: Expect::Err("markets needs a system or station name (or --address <id64>)"),
    },
    Case {
        name: "R50 --address is parsed before the missing-name check",
        argv: &["markets", "--address", "abc"],
        env: &[],
        expect: Expect::Err("--address must be an unsigned decimal integer"),
    },
];

#[test]
fn markets_config_cases() {
    run(MARKETS_CASES, |cli| {
        config::markets_config(cli).map_err(|error| error.message().to_owned())
    });
}

/// `--cached-timestamp` is honoured by `markets` and ignored by the sweep
/// \[R51\] — including its validation, which the sweep never performs.
#[test]
fn the_cached_timestamp_is_only_read_by_markets() {
    let honoured = with_cli(&["markets", "--cached-timestamp", "17", "--language", "fr"], &[], |cli| {
        config::starsystem_query(cli, CachedTimestamp::Flag).unwrap()
    });
    assert_eq!(honoured.cached_timestamp, 17.0);
    assert_eq!(honoured.language, "fr");

    let defaulted =
        with_cli(&["markets"], &[], |cli| config::starsystem_query(cli, CachedTimestamp::Flag).unwrap());
    assert_eq!(defaulted.cached_timestamp, 0.0);
    assert_eq!(defaulted.language, "en");

    assert!(with_cli(&["markets", "--cached-timestamp", "x"], &[], |cli| {
        config::starsystem_query(cli, CachedTimestamp::Flag).is_err()
    }));
    let swept = with_cli(&["market", "--cached-timestamp", "x"], &[], |cli| {
        config::starsystem_query(cli, CachedTimestamp::SweepZero).unwrap()
    });
    assert_eq!(swept.cached_timestamp, 0.0, "the sweep hardcodes 0 and never parses the flag");
}

// ---------------------------------------------------------------------------
// loadEddnOptions
// ---------------------------------------------------------------------------

#[test]
fn eddn_options_default_to_the_commander_and_the_constants() {
    let credentials = config::Credentials {
        commander_id: "F1234567".to_owned(),
        ..config::Credentials::default()
    };
    let defaults =
        with_cli(&["market"], &[], |cli| config::eddn_config(cli, &credentials).unwrap());
    assert_eq!(defaults.uploader_id, "F1234567");
    assert_eq!(defaults.software_name, "int-market-sync");
    assert_eq!(defaults.software_version, "1.0.0");
    assert_eq!(defaults.game_version, "CAPI-Live-market");
    assert_eq!(defaults.game_build, "");
    assert!(!defaults.test);
    assert_eq!(defaults.horizons, None, "absent is not false: the key is omitted");
    assert_eq!(defaults.odyssey, None);

    let overridden = with_cli(
        &[
            "market",
            "--eddn-test",
            "--uploader",
            "Jameson",
            "--software-name",
            "edm",
            "--software-version",
            "2.0",
            "--game-version",
            "4.1",
            "--game-build",
            "r300",
            "--no-horizons",
            "--odyssey",
        ],
        &[],
        |cli| config::eddn_config(cli, &credentials).unwrap(),
    );
    assert!(overridden.test);
    assert_eq!(overridden.uploader_id, "Jameson");
    assert_eq!(overridden.software_name, "edm");
    assert_eq!(overridden.software_version, "2.0");
    assert_eq!(overridden.game_version, "4.1");
    assert_eq!(overridden.game_build, "r300");
    assert_eq!(overridden.horizons, Some(false));
    assert_eq!(overridden.odyssey, Some(true));
}

// ---------------------------------------------------------------------------
// runTrade / splitItems
// ---------------------------------------------------------------------------

const DISPATCH_CASES: &[Case<TradeDispatch>] = &[
    Case {
        name: "R54 a trailing comma splits to one item and takes the single path",
        argv: &["trade", "--item", "gold,"],
        env: &[],
        expect: Expect::Ok(|dispatch| {
            assert_eq!(dispatch.items, ["gold"]);
            assert!(!dispatch.batch, "resolveTrade will then look up the *unsplit* `gold,`");
        }),
    },
    Case {
        name: "two items are a batch",
        argv: &["trade", "--item", "gold, silver"],
        env: &[],
        expect: Expect::Ok(|dispatch| {
            assert_eq!(dispatch.items, ["gold", "silver"], "each token is js-trimmed");
            assert!(dispatch.batch);
        }),
    },
    Case {
        name: "--fill forces the batch path for one item",
        argv: &["trade", "--item", "gold", "--fill"],
        env: &[],
        expect: Expect::Ok(|dispatch| assert!(dispatch.batch)),
    },
    Case {
        name: "--watch forces the batch path for one item",
        argv: &["trade", "--item", "gold", "--watch"],
        env: &[],
        expect: Expect::Ok(|dispatch| assert!(dispatch.batch)),
    },
    Case {
        name: "R47 with two items neither --fill nor --watch is read",
        argv: &["trade", "--item", "a,b", "--fill", "constructor"],
        env: &[],
        expect: Expect::Ok(|dispatch| assert!(dispatch.batch)),
    },
    Case {
        name: "R47 with one item the poisoned --fill detonates",
        argv: &["trade", "--item", "a", "--fill", "constructor"],
        env: &[],
        expect: Expect::Err(edm_core::cli::POISON_TYPE_ERROR),
    },
    Case {
        name: "ts:2323 a list of separators is empty",
        argv: &["trade", "--item", ", ,"],
        env: &[],
        expect: Expect::Err("--item needs at least one commodity"),
    },
    Case {
        name: "--item is required before anything is split",
        argv: &["trade"],
        env: &[],
        expect: Expect::Err("Missing required option --item"),
    },
];

#[test]
fn trade_dispatch_cases() {
    run(DISPATCH_CASES, |cli| {
        config::trade_dispatch(cli).map_err(|error| error.message().to_owned())
    });
}

const TRADE_INPUT_CASES: &[Case<TradeInputs>] = &[
    Case {
        name: "resolving needs a market id",
        argv: &["trade"],
        env: &[],
        expect: Expect::Err("Missing --market-id (or MARKET_ID in the environment)"),
    },
    Case {
        name: "R53 the market id is not parsed, only required",
        argv: &["trade", "--market-id", "0004306502403"],
        env: &[],
        expect: Expect::Ok(|inputs| {
            assert!(inputs.resolve);
            assert_eq!(inputs.market_id.as_deref(), Some("0004306502403"));
        }),
    },
    Case {
        name: "--no-resolve never asks for a market id here",
        argv: &["trade", "--no-resolve"],
        env: &[],
        expect: Expect::Ok(|inputs| {
            assert!(!inputs.resolve);
            assert_eq!(inputs.market_id, None);
        }),
    },
];

#[test]
fn trade_input_cases() {
    run(TRADE_INPUT_CASES, |cli| {
        config::trade_inputs(cli).map_err(|error| error.message().to_owned())
    });
}

// ---------------------------------------------------------------------------
// resolveTrade
// ---------------------------------------------------------------------------

/// A market with one ordinary good, one illegal one, one that is not sold, one
/// that is sold out, and a hold carrying ten Gold and seven Silver — seventeen
/// units in all.
const LISTING: &str = r#"{
  "commodities": {
    "a": { "id": 128049204, "name": "Gold", "categoryname": "Metals", "stock": 50,
           "buyPrice": 100, "sellPrice": 90, "fencePrice": 40, "demand": 5, "meanPrice": 95,
           "legality": "" },
    "b": { "id": 128049240, "name": "BasicNarcotics", "categoryname": "Narcotics", "stock": 12,
           "buyPrice": 700, "sellPrice": 600, "fencePrice": 900, "demand": 0, "meanPrice": 650,
           "legality": "illegal" },
    "c": { "id": 128049166, "name": "Water", "categoryname": "Chemicals", "stock": 0,
           "buyPrice": 0, "sellPrice": 3, "fencePrice": 1, "demand": 40, "meanPrice": 5,
           "legality": "" },
    "d": { "id": 128049202, "name": "Tea", "categoryname": "Foods", "stock": 0,
           "buyPrice": 500, "sellPrice": 400, "fencePrice": 200, "demand": 2, "meanPrice": 450,
           "legality": "" }
  },
  "inventory": [
    { "commodity": "Gold", "qty": 10, "stolen": false },
    { "commodity": "Silver", "qty": 7, "stolen": false }
  ]
}"#;

fn listing() -> JsValue {
    JsValue::parse(LISTING).expect("the fixture is valid JSON")
}

fn resolve(
    argv: &[&str],
    env: &[(&str, &str)],
    snapshot: Option<&MarketSnapshot<'_>>,
) -> Result<ResolvedTrade, String> {
    with_cli(argv, env, |cli| {
        config::resolve_trade(cli, snapshot).map_err(|error| error.message().to_owned())
    })
}

/// Runs a case against the fixture market.
fn resolved(argv: &[&str], env: &[(&str, &str)]) -> Result<ResolvedTrade, String> {
    let document = listing();
    let snapshot = parse_market_snapshot(&document).expect("the fixture is a market listing");
    resolve(argv, env, Some(&snapshot))
}

/// Runs a case with no listing at all — the `--no-resolve` path.
fn unresolved(argv: &[&str], env: &[(&str, &str)]) -> Result<ResolvedTrade, String> {
    resolve(argv, env, None)
}

fn field<'a>(resolved: &'a ResolvedTrade, label: &str) -> (&'a str, PlanSource) {
    let found = resolved
        .fields
        .iter()
        .find(|candidate| candidate.label == label)
        .unwrap_or_else(|| panic!("no plan field {label}"));
    (found.value.as_str(), found.source)
}

#[test]
fn a_resolved_buy_prices_itself_from_the_market() {
    let plan = resolved(&["trade", "--market-id", "3223343616", "--type", "buy", "--item", "gold", "--qty", "10"], &[])
        .unwrap();
    assert_eq!(plan.plan.kind, Kind::Buy);
    assert_eq!(plan.plan.commodity_id, 128_049_204.0);
    assert_eq!(plan.plan.commodity_name, "Gold");
    assert_eq!(plan.plan.unit_price, 100.0);
    assert_eq!(plan.plan.qty, 10.0);
    assert_eq!(plan.plan.final_qty, 20.0, "held 10 + bought 10");
    assert!(!plan.plan.black_market);

    assert_eq!(
        plan.fields.iter().map(|f| f.label).collect::<Vec<_>>(),
        [
            "marketId",
            "transactionType",
            "commodityId",
            "blackMarket",
            "stolen",
            "unitPrice",
            "qty",
            "finalQty",
            "total"
        ]
    );
    assert_eq!(field(&plan, "marketId"), ("3223343616", PlanSource::Flag));
    assert_eq!(field(&plan, "commodityId"), ("128049204 (Gold)", PlanSource::Market));
    assert_eq!(field(&plan, "unitPrice"), ("100", PlanSource::Market));
    assert_eq!(field(&plan, "qty"), ("10", PlanSource::Flag));
    assert_eq!(field(&plan, "finalQty"), ("20", PlanSource::Market));
    assert_eq!(field(&plan, "total"), ("1,000 cr", PlanSource::Default));
    assert_eq!(field(&plan, "stolen"), ("0", PlanSource::Default));
    assert_eq!(
        plan.notes,
        ["Gold: stock 50 | demand 5 | buy 100 | sell 90 | fence 40 | held 10"]
    );
}

/// The market id read for provenance omits the environment fallback, so an id
/// that came from `MARKET_ID` reports `default`.
#[test]
fn the_market_id_provenance_ignores_the_environment() {
    let plan = resolved(
        &["trade", "--type", "buy", "--item", "gold", "--qty", "1"],
        &[("MARKET_ID", "3223343616")],
    )
    .unwrap();
    assert_eq!(field(&plan, "marketId"), ("3223343616", PlanSource::Default));
}

#[test]
fn a_numeric_item_is_flag_provenance_and_a_name_is_market() {
    let by_id = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "128049204", "--qty", "1"],
        &[],
    )
    .unwrap();
    assert_eq!(field(&by_id, "commodityId"), ("128049204 (Gold)", PlanSource::Flag));
}

#[test]
fn the_stock_clamp_notes_what_it_clamped_to() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "999"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.qty, 50.0);
    assert_eq!(field(&plan, "qty"), ("50", PlanSource::Market));
    assert_eq!(plan.notes[0], "--qty 999 clamped to stock 50");
}

#[test]
fn the_free_space_clamp_measures_against_what_is_already_aboard() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "10", "--cargo", "20"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.qty, 3.0, "20 capacity less 17 aboard");
    assert_eq!(plan.notes[0], "qty 10 clamped to free cargo space 3");

    let full = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "10", "--cargo", "17"],
        &[],
    );
    assert_eq!(full.unwrap_err(), "Cargo is full (17 units); nothing can be bought");
}

/// Both clamps fire, and their notes appear in clamp order.
#[test]
fn the_two_clamps_stack() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "999", "--cargo", "20"],
        &[],
    )
    .unwrap();
    assert_eq!(
        plan.notes[..2],
        [
            "--qty 999 clamped to stock 50".to_owned(),
            "qty 50 clamped to free cargo space 3".to_owned(),
        ]
    );
    assert_eq!(plan.plan.qty, 3.0);
}

/// **R50/R94.** `--cargo` is parsed only after the stock clamp has had its
/// chance to throw, so an empty market reports its empty stock and a malformed
/// `--cargo` is never seen.
#[test]
fn a_bad_cargo_surfaces_after_the_stock_clamp() {
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "tea", "--qty", "1", "--cargo", "abc"],
            &[],
        )
        .unwrap_err(),
        "Tea: stock is 0, nothing to buy. Pass --no-cap to send the request anyway."
    );
    // With stock to clamp against, the same `--cargo` is reached and rejected.
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--cargo", "abc"],
            &[],
        )
        .unwrap_err(),
        "--cargo must be an unsigned decimal integer"
    );
    // And under `--no-cap` the whole branch is skipped, so `--cargo` is never
    // parsed at all.
    let ignored = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--cargo", "abc", "--no-cap"],
        &[],
    )
    .unwrap();
    assert_eq!(ignored.plan.qty, 1.0);
}

/// `--final-qty` is parsed last of all, so every clamp gets to fail first.
#[test]
fn a_bad_final_qty_surfaces_last() {
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--cargo", "17", "--final-qty", "abc"],
            &[],
        )
        .unwrap_err(),
        "Cargo is full (17 units); nothing can be bought"
    );
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--final-qty", "abc"],
            &[],
        )
        .unwrap_err(),
        "--final-qty must be an unsigned decimal integer"
    );
    let explicit = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--final-qty", "500"],
        &[],
    )
    .unwrap();
    assert_eq!(explicit.plan.final_qty, 500.0);
    assert_eq!(field(&explicit, "finalQty"), ("500", PlanSource::Flag));
}

#[test]
fn selling_clamps_to_the_hold_and_empties_the_stack() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "gold", "--qty", "100"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.qty, 10.0);
    assert_eq!(plan.plan.unit_price, 90.0);
    assert_eq!(plan.plan.final_qty, 0.0);
    assert_eq!(plan.notes[0], "--qty 100 clamped to holdings 10");

    let stolen = resolved(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "gold", "--qty", "1", "--stolen"],
        &[],
    );
    assert_eq!(
        stolen.unwrap_err(),
        "Gold: stolen holdings is 0, nothing to sell. Pass --no-cap to send the request anyway."
    );
}

/// Cargo space never constrains a sale, so `--cargo` is not even parsed on the
/// sell path once the stock clamp is past.
#[test]
fn a_sale_ignores_cargo_entirely() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "gold", "--qty", "5", "--cargo", "1"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.qty, 5.0);
}

#[test]
fn illegal_goods_route_through_the_black_market() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "narcotics", "--qty", "1", "--no-cap"],
        &[],
    )
    .unwrap();
    assert!(plan.plan.black_market);
    assert_eq!(plan.plan.unit_price, 900.0, "a fence pays differently");
    assert_eq!(field(&plan, "blackMarket"), ("1", PlanSource::Market));

    let forced = resolved(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "narcotics", "--qty", "1", "--no-cap", "--no-black-market"],
        &[],
    )
    .unwrap();
    assert!(!forced.plan.black_market);
    assert_eq!(forced.plan.unit_price, 600.0);
    assert_eq!(field(&forced, "blackMarket"), ("0", PlanSource::Flag));
}

#[test]
fn a_commodity_the_market_does_not_sell_names_itself() {
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "water", "--qty", "1", "--no-cap"],
            &[],
        )
        .unwrap_err(),
        "Water is not sold at this market (buyPrice 0)"
    );
}

#[test]
fn an_explicit_price_beats_the_market_and_is_flag_provenance() {
    let plan = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1", "--unit-price", "7"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.unit_price, 7.0);
    assert_eq!(field(&plan, "unitPrice"), ("7", PlanSource::Flag));
    assert_eq!(field(&plan, "total"), ("7 cr", PlanSource::Default));
}

#[test]
fn resolve_trade_read_order() {
    // Everything after `--market-id` is wrong; the market id is read first.
    assert_eq!(
        resolved(&["trade", "--type", "nonsense", "--qty", "0"], &[]).unwrap_err(),
        "Missing --market-id (or MARKET_ID in the environment)"
    );
    // Then the type, before `--item`.
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "nonsense"], &[]).unwrap_err(),
        "--type must be buy or sell, not \"nonsense\""
    );
    // `--type` is lowercased before the test.
    assert!(
        resolved(&["trade", "--market-id", "1", "--type", "BUY", "--item", "gold", "--qty", "1"], &[])
            .is_ok()
    );
    // Then the item, before `--qty`.
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "buy", "--qty", "0"], &[]).unwrap_err(),
        "Missing required option --item"
    );
    // Then qty's presence, then its value.
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "gold"], &[]).unwrap_err(),
        "Missing required option --qty"
    );
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "0"], &[])
            .unwrap_err(),
        "--qty must be at least 1"
    );
    // Then `--unit-price`, before the commodity lookup — a bad price beats an
    // unknown commodity.
    assert_eq!(
        resolved(
            &["trade", "--market-id", "1", "--type", "buy", "--item", "unobtainium", "--qty", "1", "--unit-price", "x"],
            &[],
        )
        .unwrap_err(),
        "--unit-price must be an unsigned decimal integer"
    );
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "unobtainium", "--qty", "1"], &[])
            .unwrap_err(),
        "No commodity matching \"unobtainium\" at this market"
    );
}

/// R93: the separator strip applies to the needle only, so a full name with a
/// space can never match.
#[test]
fn find_commodity_quirks_reach_the_plan() {
    let ambiguous = resolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "a", "--qty", "1"],
        &[],
    );
    assert_eq!(
        ambiguous.unwrap_err(),
        "\"a\" matches 3 commodities: BasicNarcotics, Water, Tea",
        "`a` is a substring of three of the four names, listed in key order [R5]"
    );
    assert_eq!(
        resolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "999", "--qty", "1"], &[])
            .unwrap_err(),
        "No commodity with id 999 at this market"
    );
}

// --- the --no-resolve path -------------------------------------------------

#[test]
fn an_unresolved_trade_needs_a_numeric_id_and_a_price() {
    assert_eq!(
        unresolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "gold", "--qty", "1"], &[])
            .unwrap_err(),
        "--item must be a numeric id when --no-resolve is used"
    );
    assert_eq!(
        unresolved(&["trade", "--market-id", "1", "--type", "buy", "--item", "128049204", "--qty", "1"], &[])
            .unwrap_err(),
        "--unit-price is required when --no-resolve is used"
    );

    let plan = unresolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "128049204", "--qty", "9", "--unit-price", "1000"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.commodity_name, "id 128049204");
    assert_eq!(plan.plan.final_qty, 9.0, "no listing, so finalQty falls back to qty");
    assert_eq!(field(&plan, "commodityId"), ("128049204 (id 128049204)", PlanSource::Flag));
    assert_eq!(field(&plan, "finalQty"), ("9", PlanSource::Default));
    assert_eq!(field(&plan, "blackMarket"), ("0", PlanSource::Market));
    assert_eq!(field(&plan, "total"), ("9,000 cr", PlanSource::Default));
    assert_eq!(
        plan.notes,
        ["--no-resolve: finalQty falls back to qty, which is only right if you hold none of this commodity"]
    );
}

/// Without a listing there is nothing to clamp against, so `--cargo` is never
/// parsed on this path either.
#[test]
fn an_unresolved_trade_never_clamps() {
    let plan = unresolved(
        &["trade", "--market-id", "1", "--type", "buy", "--item", "1", "--qty", "9999999", "--unit-price", "1", "--cargo", "abc"],
        &[],
    )
    .unwrap();
    assert_eq!(plan.plan.qty, 9_999_999.0);
}

// ---------------------------------------------------------------------------
// loadBatchSettings
// ---------------------------------------------------------------------------

fn batch(argv: &[&str], env: &[(&str, &str)]) -> Result<BatchConfig, String> {
    with_cli(argv, env, |cli| {
        config::batch_config(cli, vec!["gold".to_owned()])
            .map_err(|error| error.message().to_owned())
    })
}

const BATCH_CASES: &[Case<BatchConfig>] = &[
    Case {
        name: "ts:2054 the type is validated first",
        argv: &["trade", "--type", "give", "--fill"],
        env: &[],
        expect: Expect::Err("--type must be buy or sell, not \"give\""),
    },
    Case {
        name: "ts:2063 --fill only applies to buy",
        argv: &["trade", "--type", "sell", "--fill", "--cargo", "100"],
        env: &[],
        expect: Expect::Err("--fill only applies to --type buy"),
    },
    Case {
        name: "ts:2064 --fill needs a capacity",
        argv: &["trade", "--type", "buy", "--fill"],
        env: &[],
        expect: Expect::Err("--fill needs --cargo <capacity> to know when the hold is full"),
    },
    Case {
        name: "ts:2065 --fill cannot combine with --no-cap",
        argv: &["trade", "--type", "buy", "--fill", "--cargo", "100", "--no-cap"],
        env: &[],
        expect: Expect::Err("--fill cannot be combined with --no-cap"),
    },
    Case {
        name: "ts:2066 --qty is required without --fill",
        argv: &["trade", "--type", "buy"],
        env: &[],
        expect: Expect::Err("Missing required option --qty (or pass --fill)"),
    },
    Case {
        name: "ts:2067 --qty 0 is rejected even under --fill",
        argv: &["trade", "--type", "buy", "--fill", "--cargo", "100", "--qty", "0"],
        env: &[],
        expect: Expect::Err("--qty must be at least 1"),
    },
    Case {
        name: "ts:2068 --no-resolve is refused by every batch run",
        argv: &["trade", "--type", "buy", "--qty", "5", "--no-resolve"],
        env: &[],
        expect: Expect::Err("--no-resolve cannot be used with --fill or multiple items"),
    },
    Case {
        name: "ts:2070 --watch needs a stopping condition",
        argv: &["trade", "--type", "buy", "--qty", "5", "--watch"],
        env: &[],
        expect: Expect::Err("--watch needs --fill (or --attempts <n>) so it has a stopping condition"),
    },
    Case {
        name: "--watch with --attempts is allowed",
        argv: &["trade", "--type", "buy", "--qty", "5", "--watch", "--attempts", "3", "--market-id", "1"],
        env: &[],
        expect: Expect::Ok(|settings| {
            assert!(settings.watch);
            assert_eq!(settings.attempt_limit, 3.0);
        }),
    },
    Case {
        name: "ts:2072 --interval has a floor",
        argv: &["trade", "--type", "buy", "--qty", "5", "--interval", "0.05"],
        env: &[],
        expect: Expect::Err("--interval must be between 0.1 and 3600 seconds"),
    },
    Case {
        name: "ts:2072 --interval has a ceiling",
        argv: &["trade", "--type", "buy", "--qty", "5", "--interval", "3601"],
        env: &[],
        expect: Expect::Err("--interval must be between 0.1 and 3600 seconds"),
    },
    Case {
        name: "ts:1012 a zero interval is caught by optionalDecimal first",
        argv: &["trade", "--type", "buy", "--qty", "5", "--interval", "0"],
        env: &[],
        expect: Expect::Err("--interval must be a positive number"),
    },
    Case {
        name: "R50 the market id is required after every guard",
        argv: &["trade", "--type", "buy", "--qty", "5", "--interval", "0.05"],
        env: &[],
        expect: Expect::Err("--interval must be between 0.1 and 3600 seconds"),
    },
    Case {
        name: "R50 and before --stolen, --black-market, --unit-price and --credits",
        argv: &["trade", "--type", "buy", "--qty", "5", "--credits", "abc"],
        env: &[],
        expect: Expect::Err("Missing --market-id (or MARKET_ID in the environment)"),
    },
    Case {
        name: "R53 the batch market id is not parsed",
        argv: &["trade", "--type", "buy", "--qty", "5", "--market-id", "0004306502403"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.market_id, "0004306502403")),
    },
    Case {
        name: "a fully specified batch",
        argv: &[
            "trade", "--market-id", "1", "--type", "buy", "--fill", "--cargo", "784", "--stolen",
            "--black-market", "--unit-price", "12", "--attempts", "9", "--interval", "1.5",
            "--credits", "1000000",
        ],
        env: &[],
        expect: Expect::Ok(|settings| {
            assert_eq!(settings.kind, Kind::Buy);
            assert_eq!(settings.items, ["gold"]);
            assert!(settings.fill);
            assert_eq!(settings.cargo, Some(784.0));
            assert_eq!(settings.per_item_qty, None);
            assert!(settings.stolen);
            assert_eq!(settings.explicit_black_market, Some(true));
            assert_eq!(settings.explicit_price, Some(12.0));
            assert!(!settings.watch);
            assert_eq!(settings.interval_ms, 1500.0);
            assert_eq!(settings.attempt_limit, 9.0);
            assert_eq!(settings.credits, Some(1_000_000.0));
        }),
    },
    Case {
        name: "the interval default is one second",
        argv: &["trade", "--market-id", "1", "--type", "buy", "--qty", "5"],
        env: &[],
        expect: Expect::Ok(|settings| {
            assert_eq!(settings.interval_ms, 1000.0);
            assert_eq!(settings.attempt_limit, 0.0);
            assert_eq!(settings.credits, None);
            assert_eq!(settings.per_item_qty, Some(5.0));
        }),
    },
    Case {
        name: "R12 the interval rounds half toward +Infinity",
        argv: &["trade", "--market-id", "1", "--type", "buy", "--qty", "5", "--interval", "0.1005"],
        env: &[],
        expect: Expect::Ok(|settings| assert_eq!(settings.interval_ms, 101.0)),
    },
];

#[test]
fn batch_config_cases() {
    for case in BATCH_CASES {
        let outcome = batch(case.argv, case.env);
        match (&case.expect, outcome) {
            (Expect::Err(expected), Err(actual)) => assert_eq!(&actual, expected, "{}", case.name),
            (Expect::Err(expected), Ok(_)) => {
                panic!("{}: expected error {expected:?}, got a value", case.name)
            }
            (Expect::Ok(_), Err(actual)) => {
                panic!("{}: expected a value, got error {actual:?}", case.name)
            }
            (Expect::Ok(check), Ok(value)) => check(&value),
        }
    }
}

/// The guards run before the market id is required, so a batch that is wrong in
/// two ways always reports the guard \[R50\].
#[test]
fn every_batch_guard_precedes_the_market_id() {
    let guards: &[&[&str]] = &[
        &["trade", "--type", "sell", "--fill", "--cargo", "1"],
        &["trade", "--type", "buy", "--fill"],
        &["trade", "--type", "buy", "--fill", "--cargo", "1", "--no-cap"],
        &["trade", "--type", "buy"],
        &["trade", "--type", "buy", "--qty", "0"],
        &["trade", "--type", "buy", "--qty", "1", "--no-resolve"],
        &["trade", "--type", "buy", "--qty", "1", "--watch"],
        &["trade", "--type", "buy", "--qty", "1", "--interval", "3601"],
    ];
    for argv in guards {
        let message = batch(argv, &[]).unwrap_err();
        assert_ne!(
            message, "Missing --market-id (or MARKET_ID in the environment)",
            "{argv:?} should report its guard, not the market id"
        );
    }
}

// ---------------------------------------------------------------------------
// route
// ---------------------------------------------------------------------------

fn route(argv: &[&str]) -> Result<config::RouteConfig, CliError> {
    let owned: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
    let parsed = parse_with(&owned, Table::Extended).expect("parses");
    let env = EnvSnapshot::empty();
    config::route_config(&Cli::new(&parsed, &env))
}

#[test]
fn route_needs_somewhere_to_search_around() {
    // Failing before anything is printed is the point: every other flag is
    // meaningless without a reference.
    let error = route(&["route", "--radius", "30"]).unwrap_err();
    assert_eq!(error.message(), "route needs a system or station name to search around");

    assert_eq!(route(&["route", "Sol"]).unwrap().reference, "Sol");
    assert_eq!(route(&["route", "--system", "Sol"]).unwrap().reference, "Sol");
    // A quoted multi-word name arrives as several positionals.
    assert_eq!(route(&["route", "Hyades", "Sector", "NI-X"]).unwrap().reference, "Hyades Sector NI-X");
}

/// The defaults are the whole safety story: they are what keep a nearby sweep
/// inside the ceiling, and they are load-bearing rather than cosmetic.
#[test]
fn the_defaults_exclude_what_cannot_be_traded_at() {
    let config = route(&["route", "Sol"]).unwrap();
    assert_eq!(config.radius_ly, 30.0);
    assert_eq!(config.pad, config::Pad::Large);
    assert!(!config.include_carriers, "carriers jump between planning and flying");
    assert!(!config.include_settlements, "a settlement cannot berth a large ship at all");
    assert_eq!(config.max_star_distance_ls, Some(2_000.0));
    assert_eq!(config.shape, config::Shape::RoundTrip);
    assert!(config.cache, "--no-cache comes from the parser's own negation");
}

#[test]
fn shapes_parse_including_the_bounded_loop() {
    use config::Shape;
    assert_eq!(route(&["route", "Sol", "--shape", "one-way"]).unwrap().shape, Shape::OneWay);
    assert_eq!(route(&["route", "Sol", "--shape", "roundtrip"]).unwrap().shape, Shape::RoundTrip);
    assert_eq!(route(&["route", "Sol", "--shape", "loop"]).unwrap().shape, Shape::Loop);
    assert_eq!(route(&["route", "Sol", "--shape", "loop:4"]).unwrap().shape, Shape::BoundedLoop(4));

    // A one-stop loop is not a loop.
    assert_eq!(
        route(&["route", "Sol", "--shape", "loop:1"]).unwrap_err().message(),
        "--shape loop:N needs at least 2 stops"
    );
    assert_eq!(
        route(&["route", "Sol", "--shape", "spiral"]).unwrap_err().message(),
        "--shape must be one-way, round-trip, loop or loop:N, not \"spiral\""
    );
}

#[test]
fn pads_parse_by_letter_word_or_number() {
    use config::Pad;
    for (raw, expected) in
        [("L", Pad::Large), ("large", Pad::Large), ("3", Pad::Large), ("m", Pad::Medium), ("S", Pad::Small)]
    {
        assert_eq!(route(&["route", "Sol", "--pad", raw]).unwrap().pad, expected, "{raw}");
    }
    assert_eq!(
        route(&["route", "Sol", "--pad", "XL"]).unwrap_err().message(),
        "--pad must be S, M or L, not \"xl\""
    );
}

/// `--no-cache` is not a flag: it is the parser's `--no-` negation of `--cache`,
/// which is why the flag is named for the positive.
#[test]
fn cache_is_negated_by_the_parser_not_by_a_second_flag() {
    assert!(route(&["route", "Sol"]).unwrap().cache);
    assert!(!route(&["route", "Sol", "--no-cache"]).unwrap().cache);
    assert!(route(&["route", "Sol", "--cache"]).unwrap().cache);
}

/// Route-only names must stay invisible to every other command, or the base
/// grammar has been widened and the parity contract broken \[C26\].
#[test]
fn route_flags_do_not_leak_into_other_commands() {
    let argv: Vec<String> =
        ["market", "Colonia", "--radius", "30"].iter().map(|s| (*s).to_owned()).collect();
    let error = parse(&argv).unwrap_err();
    assert_eq!(error.to_string(), "Unknown option --radius");
}

/// `route` reuses `--concurrency` rather than inventing a name, and takes the
/// ported sweep's clamp with it. The two mean the same thing — how much
/// latency is hidden — and a second name for it would be a second thing to get
/// wrong.
#[test]
fn route_reuses_the_concurrency_flag_and_its_clamp() {
    assert_eq!(route(&["route", "Sol"]).unwrap().workers, 5);
    assert_eq!(route(&["route", "Sol", "--concurrency", "3"]).unwrap().workers, 3);
    assert_eq!(route(&["route", "Sol", "--concurrency", "0"]).unwrap().workers, 1);
    assert_eq!(route(&["route", "Sol", "--concurrency", "99"]).unwrap().workers, 16);
}

/// `-v` is a flag under the extended table and a positional everywhere else.
///
/// The ported grammar knows exactly one single-dash token, `-h`; every other
/// `-x` becomes a positional so that `--qty -5` can take a negative value
/// (R44). Adding a second one to the base table would change what
/// `edm market -v` means.
#[test]
fn dash_v_is_verbose_for_route_and_nothing_elsewhere() {
    assert!(route(&["route", "Sol", "-v"]).unwrap().verbose);
    assert!(!route(&["route", "Sol"]).unwrap().verbose);
    assert!(route(&["route", "Sol", "--verbose"]).unwrap().verbose);

    // The base table leaves it a positional, exactly as the TypeScript does.
    let base = parse_with(&["market".to_owned(), "-v".to_owned()], Table::Base).expect("parses");
    assert_eq!(base.positionals, vec!["-v".to_owned()]);
}

/// The bug this pair of fixes exists for: `route Sol -v` searched Ardent for a
/// system called "Sol -v" and reported that it had never heard of it, because
/// route joins its positionals into the reference and an unrecognised
/// single-dash token is a positional. No Elite system name begins with a
/// hyphen, so a stray one is a mistyped flag and is named as one.
#[test]
fn a_stray_dash_token_is_an_unknown_option_not_part_of_a_name() {
    let error = route(&["route", "Sol", "-x"]).unwrap_err();
    assert_eq!(error.message(), "Unknown option -x");

    let error = route(&["route", "Sol", "-vv"]).unwrap_err();
    assert_eq!(error.message(), "Unknown option -vv");

    // And a name with spaces still works, which is why they are joined at all.
    assert_eq!(route(&["route", "Alpha", "Centauri"]).unwrap().reference, "Alpha Centauri");
}
