//! Vectors and properties for the command-line grammar.
//!
//! The vector table is the primary artefact: each row is a command line and
//! either the exact error text it must produce or a set of facts about the
//! parse. Rows are named after the `PORTING.md` row they defend so a failure
//! points at the contract rather than at the implementation.
//!
//! None of the expectations below were reasoned out. Every row, and a further
//! 8,624 generated command lines and 869 generated flag/environment pairs, were
//! diffed against `game-internal-api.ts`'s own `parseArguments` and accessors
//! executed under bun 1.2.3 — the same build the `js` fixtures are blessed
//! from. The generator lives outside the crate because `edm-core` may not
//! depend on a JavaScript runtime; `cargo xtask parity` is where that harness
//! belongs permanently.

use edm_core::cli::{
    self, ArgError, Args, Cli, EnvSnapshot, Flag, POISON_TYPE_ERROR, Table, Value, boolean_literal,
    normalize, usage,
};
use edm_core::js;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Vector table
// ---------------------------------------------------------------------------

/// One assertion about a successful parse.
#[derive(Debug)]
enum Check {
    Command(&'static str),
    Positionals(&'static [&'static str]),
    Text(Flag, &'static str),
    Switch(Flag, bool),
    Poisoned(Flag),
    Absent(Flag),
}

#[derive(Debug)]
enum Expect {
    /// The exact `Error.message` the TypeScript throws.
    Err(&'static str),
    Ok(&'static [Check]),
}

#[derive(Debug)]
struct Case {
    name: &'static str,
    argv: &'static [&'static str],
    expect: Expect,
}

const CASES: &[Case] = &[
    // --- R38: `--` is an option, not a terminator; `-h` is the only short flag
    Case {
        name: "R38 double dash alone is an unknown option",
        argv: &["--"],
        expect: Expect::Err("Unknown option --"),
    },
    Case {
        name: "R38 double dash does not terminate options",
        argv: &["market", "--", "Colonia"],
        expect: Expect::Err("Unknown option --"),
    },
    Case {
        name: "R38 bare --= names the empty option",
        argv: &["market", "--=x"],
        expect: Expect::Err("Unknown option --"),
    },
    Case {
        name: "R38 -h sets help",
        argv: &["-h"],
        expect: Expect::Ok(&[Check::Command("market"), Check::Switch(Flag::Help, true)]),
    },
    Case {
        name: "R38 -h is case sensitive and -H becomes the command",
        argv: &["-H"],
        expect: Expect::Ok(&[Check::Command("-h"), Check::Absent(Flag::Help)]),
    },
    Case {
        name: "R38 an unknown short flag becomes the command",
        argv: &["-x"],
        expect: Expect::Ok(&[Check::Command("-x"), Check::Positionals(&[])]),
    },
    Case {
        name: "R38 an unknown short flag after the command is a positional",
        argv: &["market", "-x"],
        expect: Expect::Ok(&[Check::Command("market"), Check::Positionals(&["-x"])]),
    },
    Case {
        name: "R38 -h may follow the command",
        argv: &["markets", "-h"],
        expect: Expect::Ok(&[Check::Command("markets"), Check::Switch(Flag::Help, true)]),
    },
    // --- R39: a bare switch consumes the next token only if it reads as boolean
    Case {
        name: "R39 --detail 1 consumes the 1",
        argv: &["market", "--detail", "1"],
        expect: Expect::Ok(&[Check::Switch(Flag::Detail, true), Check::Positionals(&[])]),
    },
    Case {
        name: "R39 --dry-run Colonia does not consume Colonia",
        argv: &["market", "--dry-run", "Colonia"],
        expect: Expect::Ok(&[
            Check::Switch(Flag::DryRun, true),
            Check::Positionals(&["Colonia"]),
        ]),
    },
    Case {
        name: "R39 boolean literals are matched case insensitively",
        argv: &["market", "--detail", "TRUE"],
        expect: Expect::Ok(&[Check::Switch(Flag::Detail, true), Check::Positionals(&[])]),
    },
    Case {
        name: "R39 off is a false literal",
        argv: &["market", "--detail", "off"],
        expect: Expect::Ok(&[Check::Switch(Flag::Detail, false), Check::Positionals(&[])]),
    },
    Case {
        name: "R39 0 is a false literal",
        argv: &["market", "--json", "0"],
        expect: Expect::Ok(&[Check::Switch(Flag::Json, false), Check::Positionals(&[])]),
    },
    Case {
        name: "R39 2 is not a literal",
        argv: &["market", "--detail", "2"],
        expect: Expect::Ok(&[
            Check::Switch(Flag::Detail, true),
            Check::Positionals(&["2"]),
        ]),
    },
    Case {
        name: "R39 a following flag is never a literal",
        argv: &["market", "--json", "--detail"],
        expect: Expect::Ok(&[
            Check::Switch(Flag::Json, true),
            Check::Switch(Flag::Detail, true),
        ]),
    },
    Case {
        name: "R39 a trailing switch defaults to true",
        argv: &["market", "--detail"],
        expect: Expect::Ok(&[Check::Switch(Flag::Detail, true)]),
    },
    // --- R40: `--no-` is matched on the raw name, before separators are stripped
    Case {
        name: "R40 --no-json negates",
        argv: &["market", "--no-json"],
        expect: Expect::Ok(&[Check::Switch(Flag::Json, false)]),
    },
    Case {
        name: "R40 the no- test is ASCII case insensitive",
        argv: &["market", "--NO-Json"],
        expect: Expect::Ok(&[Check::Switch(Flag::Json, false)]),
    },
    Case {
        name: "R40 --no_json is not a negation and not a flag",
        argv: &["market", "--no_json"],
        expect: Expect::Err("Unknown option --no_json"),
    },
    Case {
        name: "R40 negating a value flag names the whole raw name",
        argv: &["market", "--no-qty"],
        expect: Expect::Err("--no- may only negate a switch, not --no-qty"),
    },
    Case {
        name: "R40 --no- with an empty stem",
        argv: &["market", "--no-"],
        expect: Expect::Err("--no- may only negate a switch, not --no-"),
    },
    Case {
        name: "R40 negating an unknown name",
        argv: &["market", "--no-bogus"],
        expect: Expect::Err("--no- may only negate a switch, not --no-bogus"),
    },
    Case {
        name: "R40 a negation discards its =value",
        argv: &["market", "--no-json=true"],
        expect: Expect::Ok(&[Check::Switch(Flag::Json, false)]),
    },
    Case {
        name: "R40 a negation does not consume a following literal",
        argv: &["market", "--no-detail", "1"],
        expect: Expect::Ok(&[
            Check::Switch(Flag::Detail, false),
            Check::Positionals(&["1"]),
        ]),
    },
    // --- R41: strip every separator, then lowercase with full Unicode
    Case {
        name: "R41 KELVIN SIGN lowercases to k",
        argv: &["market", "--mar\u{212A}etid", "7"],
        expect: Expect::Ok(&[Check::Text(Flag::MarketId, "7")]),
    },
    Case {
        name: "R41 underscores and case are both ignored",
        argv: &["market", "--MARKET_ID=9"],
        expect: Expect::Ok(&[Check::Text(Flag::MarketId, "9")]),
    },
    Case {
        name: "R41 separators may appear anywhere and repeat",
        argv: &["market", "--m-a_r--k_et_id=9"],
        expect: Expect::Ok(&[Check::Text(Flag::MarketId, "9")]),
    },
    Case {
        name: "R41 a switch normalises the same way",
        argv: &["market", "--DRY_RUN"],
        expect: Expect::Ok(&[Check::Switch(Flag::DryRun, true)]),
    },
    // --- R42/R43: the `=` forms
    Case {
        name: "R42 only the first = splits",
        argv: &["market", "--item=a=b"],
        expect: Expect::Ok(&[Check::Text(Flag::Item, "a=b")]),
    },
    Case {
        name: "R43 --qty= stores the empty string",
        argv: &["market", "--qty="],
        expect: Expect::Ok(&[Check::Text(Flag::Qty, "")]),
    },
    Case {
        name: "R43 --json= is a parse error",
        argv: &["market", "--json="],
        expect: Expect::Err("--json expects true or false"),
    },
    Case {
        name: "R43 --json=maybe is a parse error naming the alias typed",
        argv: &["market", "--JSON=maybe"],
        expect: Expect::Err("--JSON expects true or false"),
    },
    Case {
        name: "R43 --json=yes is accepted",
        argv: &["market", "--json=yes"],
        expect: Expect::Ok(&[Check::Switch(Flag::Json, true)]),
    },
    // --- R44: a value flag accepts a single-dash next token
    Case {
        name: "R44 a value may start with one dash",
        argv: &["market", "--qty", "-5"],
        expect: Expect::Ok(&[Check::Text(Flag::Qty, "-5")]),
    },
    Case {
        name: "R44 a value may be a lone dash",
        argv: &["market", "--dump", "-"],
        expect: Expect::Ok(&[Check::Text(Flag::Dump, "-")]),
    },
    Case {
        name: "R44 a value may not start with two dashes",
        argv: &["market", "--qty", "--json"],
        expect: Expect::Err("--qty requires a value"),
    },
    Case {
        name: "R44 a value flag at the end of argv",
        argv: &["market", "--qty"],
        expect: Expect::Err("--qty requires a value"),
    },
    Case {
        name: "R44 an empty next token is still a value",
        argv: &["market", "--qty", ""],
        expect: Expect::Ok(&[Check::Text(Flag::Qty, "")]),
    },
    // --- R45: an empty token cannot become the command
    Case {
        name: "R45 an empty token is skipped over",
        argv: &["", "Colonia"],
        expect: Expect::Ok(&[Check::Command("colonia"), Check::Positionals(&[])]),
    },
    Case {
        name: "R45 an empty argv defaults the command",
        argv: &[],
        expect: Expect::Ok(&[Check::Command("market"), Check::Positionals(&[])]),
    },
    Case {
        name: "R45 all-empty argv still defaults the command",
        argv: &["", ""],
        expect: Expect::Ok(&[Check::Command("market"), Check::Positionals(&[])]),
    },
    Case {
        name: "R45 an empty token after the command is a positional",
        argv: &["markets", ""],
        expect: Expect::Ok(&[Check::Command("markets"), Check::Positionals(&[""])]),
    },
    Case {
        name: "R45 the command is lowercased",
        argv: &["MARKET", "Colonia"],
        expect: Expect::Ok(&[Check::Command("market"), Check::Positionals(&["Colonia"])]),
    },
    Case {
        name: "R45 a leading switch leaves the command slot open",
        argv: &["--dry-run", "Colonia"],
        expect: Expect::Ok(&[Check::Command("colonia"), Check::Positionals(&[])]),
    },
    // --- R46: parse errors name the alias the user typed
    Case {
        name: "R46 --market requires a value, not --market-id",
        argv: &["market", "--market"],
        expect: Expect::Err("--market requires a value"),
    },
    Case {
        name: "R46 an alias resolves to the canonical slot",
        argv: &["market", "--capacity", "1232"],
        expect: Expect::Ok(&[Check::Text(Flag::Cargo, "1232")]),
    },
    Case {
        name: "R46 every concurrency alias lands in one slot",
        argv: &[
            "market",
            "--rate",
            "2",
            "--workers",
            "3",
            "--jobs",
            "4",
            "--parallel",
            "5",
        ],
        expect: Expect::Ok(&[Check::Text(Flag::Concurrency, "5")]),
    },
    // --- R47: the two prototype-reachable boolean literals
    Case {
        name: "R47 constructor is consumed and poisons the slot",
        argv: &["market", "--detail", "constructor"],
        expect: Expect::Ok(&[Check::Poisoned(Flag::Detail), Check::Positionals(&[])]),
    },
    Case {
        name: "R47 __proto__ is consumed and poisons the slot",
        argv: &["market", "--detail", "__proto__"],
        expect: Expect::Ok(&[Check::Poisoned(Flag::Detail), Check::Positionals(&[])]),
    },
    Case {
        name: "R47 the lookup is case folded like every other literal",
        argv: &["market", "--detail", "Constructor"],
        expect: Expect::Ok(&[Check::Poisoned(Flag::Detail), Check::Positionals(&[])]),
    },
    Case {
        name: "R47 the =value form poisons too",
        argv: &["market", "--json=CONSTRUCTOR"],
        expect: Expect::Ok(&[Check::Poisoned(Flag::Json)]),
    },
    Case {
        name: "R47 __proto__ is not separator stripped, so --__proto__ is unknown",
        argv: &["market", "--__proto__"],
        expect: Expect::Err("Unknown option --__proto__"),
    },
    Case {
        name: "R47 hasOwnProperty does not survive lowercasing",
        argv: &["market", "--detail", "hasOwnProperty"],
        expect: Expect::Ok(&[
            Check::Switch(Flag::Detail, true),
            Check::Positionals(&["hasOwnProperty"]),
        ]),
    },
    // --- last-wins, mixed forms, and the plain happy paths
    Case {
        name: "repeats overwrite, whichever alias spelled them",
        argv: &["market", "--qty", "5", "--quantity", "7"],
        expect: Expect::Ok(&[Check::Text(Flag::Qty, "7")]),
    },
    Case {
        name: "a switch set true then negated ends false",
        argv: &["market", "--detail", "yes", "--no-detail"],
        expect: Expect::Ok(&[Check::Switch(Flag::Detail, false)]),
    },
    Case {
        name: "positionals keep their order and case",
        argv: &["trade", "--item", "a", "Beta", "gamma"],
        expect: Expect::Ok(&[
            Check::Command("trade"),
            Check::Text(Flag::Item, "a"),
            Check::Positionals(&["Beta", "gamma"]),
        ]),
    },
    Case {
        name: "an unknown option reports the raw name without its =value",
        argv: &["market", "--bogus=1"],
        expect: Expect::Err("Unknown option --bogus"),
    },
    Case {
        name: "R51 --concurrency 0 parses; the clamp to one worker is not the parser's",
        argv: &["market", "--concurrency", "0"],
        expect: Expect::Ok(&[Check::Text(Flag::Concurrency, "0")]),
    },
    Case {
        name: "a realistic sweep",
        argv: &[
            "market",
            "Colonia",
            "--eddn",
            "--concurrency",
            "8",
            "--timeout",
            "2.5",
            "--requeue",
            "1",
        ],
        expect: Expect::Ok(&[
            Check::Command("market"),
            Check::Positionals(&["Colonia"]),
            Check::Switch(Flag::Eddn, true),
            Check::Text(Flag::Concurrency, "8"),
            Check::Text(Flag::Timeout, "2.5"),
            Check::Text(Flag::Requeue, "1"),
        ]),
    },
];

fn parse(argv: &[&str]) -> Result<Args, ArgError> {
    let owned: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
    cli::parse(&owned)
}

#[test]
fn vectors() {
    let mut failures: Vec<String> = Vec::new();

    for case in CASES {
        let outcome = parse(case.argv);
        match (&case.expect, outcome) {
            (Expect::Err(want), Ok(_)) => {
                failures.push(format!(
                    "{}: expected error {want:?}, parsed successfully",
                    case.name
                ));
            }
            (Expect::Err(want), Err(got)) => {
                let got = got.to_string();
                if got != *want {
                    failures.push(format!("{}: expected {want:?}, got {got:?}", case.name));
                }
            }
            (Expect::Ok(_), Err(got)) => {
                failures.push(format!("{}: expected a parse, got error {got}", case.name));
            }
            (Expect::Ok(checks), Ok(args)) => {
                for check in *checks {
                    if let Err(message) = verify(&args, check) {
                        failures.push(format!("{}: {message}", case.name));
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} vector(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn verify(args: &Args, check: &Check) -> Result<(), String> {
    match check {
        Check::Command(want) => {
            if args.command == *want {
                Ok(())
            } else {
                Err(format!(
                    "command: expected {want:?}, got {:?}",
                    args.command
                ))
            }
        }
        Check::Positionals(want) => {
            if args.positionals == *want {
                Ok(())
            } else {
                Err(format!(
                    "positionals: expected {want:?}, got {:?}",
                    args.positionals
                ))
            }
        }
        Check::Text(flag, want) => match args.get(*flag) {
            Some(Value::Text(got)) if &**got == *want => Ok(()),
            other => Err(format!(
                "{}: expected text {want:?}, got {other:?}",
                flag.display()
            )),
        },
        Check::Switch(flag, want) => match args.get(*flag) {
            Some(Value::Bool(got)) if got == want => Ok(()),
            other => Err(format!(
                "{}: expected {want}, got {other:?}",
                flag.display()
            )),
        },
        Check::Poisoned(flag) => match args.get(*flag) {
            Some(Value::Poison) => Ok(()),
            other => Err(format!(
                "{}: expected a poisoned slot, got {other:?}",
                flag.display()
            )),
        },
        Check::Absent(flag) => match args.get(*flag) {
            None => Ok(()),
            other => Err(format!(
                "{}: expected no value, got {other:?}",
                flag.display()
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

fn env(pairs: &[(&str, &str)]) -> EnvSnapshot {
    EnvSnapshot::from_pairs(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
    )
}

#[test]
fn optional_value_trims_on_read() {
    let args = parse(&["market", "--qty", "  5  "]).unwrap();
    let env = EnvSnapshot::empty();
    assert_eq!(
        Cli::new(&args, &env).optional_value(Flag::Qty, None),
        Some("5")
    );
}

#[test]
fn r56_a_blank_flag_falls_through_to_the_environment() {
    let env = env(&[("QTY", " 12 ")]);

    // Present but empty (R43's `--qty=`), present but whitespace, and absent
    // must all reach the environment.
    for argv in [
        &["market", "--qty="][..],
        &["market", "--qty", "   "][..],
        &["market"][..],
    ] {
        let args = parse(argv).unwrap();
        assert_eq!(
            Cli::new(&args, &env).optional_value(Flag::Qty, Some("QTY")),
            Some("12"),
            "{argv:?}"
        );
    }
}

#[test]
fn a_blank_environment_variable_is_not_a_value() {
    let args = parse(&["market"]).unwrap();
    let env = env(&[("QTY", "   ")]);
    assert_eq!(
        Cli::new(&args, &env).optional_value(Flag::Qty, Some("QTY")),
        None
    );
}

#[test]
fn r55_the_environment_snapshot_is_first_wins() {
    let snapshot = env(&[("MARKET_ID", "first"), ("MARKET_ID", "second")]);
    assert_eq!(snapshot.get("MARKET_ID"), Some("first"));
}

#[test]
fn require_value_messages() {
    let args = parse(&["market"]).unwrap();
    let empty = EnvSnapshot::empty();
    let cli = Cli::new(&args, &empty);

    // ts:995 and ts:996 — the presence of an environment fallback picks the
    // message, and the flag is named by its documented spelling.
    assert_eq!(
        cli.require_value(Flag::MarketId, Some("MARKET_ID"))
            .unwrap_err()
            .to_string(),
        "Missing --market-id (or MARKET_ID in the environment)"
    );
    assert_eq!(
        cli.require_value(Flag::Type, None).unwrap_err().to_string(),
        "Missing required option --type"
    );
}

#[test]
fn r46_accessor_errors_name_the_canonical_spelling() {
    let args = parse(&["market", "--capacity", "abc"]).unwrap();
    let empty = EnvSnapshot::empty();
    // Typed `--capacity`, told about `--cargo`.
    assert_eq!(
        Cli::new(&args, &empty)
            .optional_number(Flag::Cargo)
            .unwrap_err()
            .to_string(),
        "--cargo must be an unsigned decimal integer"
    );
}

#[test]
fn optional_number_rejections() {
    let empty = EnvSnapshot::empty();
    let hundred_digits = "1".repeat(100);
    let rows: &[(&str, &str)] = &[
        ("abc", "--qty must be an unsigned decimal integer"),
        ("-1", "--qty must be an unsigned decimal integer"),
        ("1.5", "--qty must be an unsigned decimal integer"),
        // R11: a full-width digit is not `/^\d+$/`.
        ("\u{FF11}", "--qty must be an unsigned decimal integer"),
        // R11: this one passes the pattern and fails the range check, so it
        // gets the *second* message.
        (
            hundred_digits.as_str(),
            "--qty is outside the safe integer range",
        ),
    ];
    for (input, want) in rows {
        let args = parse(&["market", "--qty", input]).unwrap();
        assert_eq!(
            Cli::new(&args, &empty)
                .optional_number(Flag::Qty)
                .unwrap_err()
                .to_string(),
            *want,
            "{input:?}"
        );
    }
}

#[test]
fn optional_number_accepts_and_ignores_the_environment() {
    let args = parse(&["market", "--qty", " 42 "]).unwrap();
    let env = env(&[("QTY", "7")]);
    assert_eq!(
        Cli::new(&args, &env).optional_number(Flag::Qty).unwrap(),
        Some(42.0)
    );

    // `optionalNumber` passes no environment name (ts:1003), so a bare `--qty`
    // stays absent even with QTY set.
    let bare = parse(&["market"]).unwrap();
    assert_eq!(
        Cli::new(&bare, &env).optional_number(Flag::Qty).unwrap(),
        None
    );
}

#[test]
fn optional_decimal_uses_number_not_a_rust_float_parse() {
    let empty = EnvSnapshot::empty();
    let accepted: &[(&str, f64)] = &[
        ("1.5", 1.5),
        (" 1.5 ", 1.5),
        ("1e3", 1000.0),
        ("0x10", 16.0),
        (".5", 0.5),
    ];
    for (input, want) in accepted {
        let args = parse(&["market", "--interval", input]).unwrap();
        assert_eq!(
            Cli::new(&args, &empty)
                .optional_decimal(Flag::Interval)
                .unwrap(),
            Some(*want),
            "{input:?}"
        );
    }

    // `Infinity` parses but is not finite; `inf` and `1_0` are not numbers at
    // all; zero and negatives are rejected by the `<= 0` test. All four take
    // the same message (ts:1012).
    for input in ["0", "-1", "Infinity", "inf", "1_0", "abc", ""] {
        let args = parse(&["market", "--interval", input]).unwrap();
        let outcome = Cli::new(&args, &empty).optional_decimal(Flag::Interval);
        if input.is_empty() {
            // A blank flag is absent, not invalid (R56).
            assert_eq!(outcome.unwrap(), None);
        } else {
            assert_eq!(
                outcome.unwrap_err().to_string(),
                "--interval must be a positive number",
                "{input:?}"
            );
        }
    }
}

#[test]
fn r47_a_poisoned_switch_throws_rather_than_failing_to_parse() {
    let args = parse(&["market", "--detail", "constructor"]).unwrap();
    let empty = EnvSnapshot::empty();
    let cli = Cli::new(&args, &empty);

    // Exit 1 with an engine message, not exit 2 with a parse error — and the
    // token was consumed, so it is not a positional either.
    assert_eq!(
        cli.optional_switch(Flag::Detail).unwrap_err().to_string(),
        POISON_TYPE_ERROR
    );
    assert_eq!(
        cli.switch_value(Flag::Detail, false)
            .unwrap_err()
            .to_string(),
        POISON_TYPE_ERROR
    );
    assert!(args.positionals.is_empty());
}

#[test]
fn switch_value_falls_back() {
    let args = parse(&["market", "--json"]).unwrap();
    let empty = EnvSnapshot::empty();
    let cli = Cli::new(&args, &empty);
    assert!(cli.switch_value(Flag::Json, false).unwrap());
    assert!(cli.switch_value(Flag::Detail, true).unwrap());
    assert!(!cli.switch_value(Flag::Detail, false).unwrap());
    assert_eq!(cli.optional_switch(Flag::Detail).unwrap(), None);
}

#[test]
fn r48_help_is_reachable_from_an_unknown_command() {
    let args = parse(&["bogus", "--help"]).unwrap();
    let empty = EnvSnapshot::empty();

    // `main` tests the command, then the switch, and only then the known set,
    // so this run prints USAGE and exits 0.
    assert!(!cli::is_known_command(&args.command));
    assert!(
        Cli::new(&args, &empty)
            .switch_value(Flag::Help, false)
            .unwrap()
    );

    let help = parse(&["help"]).unwrap();
    assert_eq!(help.command, "help");
    assert!(!cli::is_known_command(&help.command));
}

// ---------------------------------------------------------------------------
// USAGE
// ---------------------------------------------------------------------------

/// Changing a constant must change the help text, which is the whole reason
/// the interpolation is reproduced rather than frozen.
#[test]
fn usage_advertises_the_constants_it_runs_on() {
    let text = usage();
    assert!(text.contains(&format!(
        "default {}, max {}",
        js::js_number(cli::usage::DEFAULT_CONCURRENCY),
        js::js_number(cli::usage::MAX_CONCURRENCY)
    )));
    assert!(text.contains(&format!(
        "per-attempt timeout, default {}",
        js::js_number(cli::usage::DEFAULT_TIMEOUT_SECONDS)
    )));
    assert!(text.contains(&format!(
        "default {} (EDDN posts",
        js::js_number(cli::usage::DEFAULT_REQUEUES)
    )));
    assert!(text.contains(cli::usage::EDDN_UPLOAD_URL));
    assert!(text.contains(cli::usage::MARKET_TRADE_PATH));
    // R49: the text carries no trailing newline; `console.log` adds it.
    assert!(!text.ends_with('\n'));
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

/// Tokens biased toward the shapes the parser actually branches on.
fn token() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => prop::string::string_regex("(--?)?[a-zA-Z_=-]{0,6}").unwrap(),
        2 => prop::sample::select(vec![
            String::new(),
            "--".to_owned(),
            "-h".to_owned(),
            "-".to_owned(),
            "--no-".to_owned(),
            "--json=".to_owned(),
            "--qty".to_owned(),
            "constructor".to_owned(),
            "__proto__".to_owned(),
            "1".to_owned(),
            "off".to_owned(),
            "--mar\u{212A}etid".to_owned(),
        ]),
        1 => any::<String>(),
    ]
}

/// Every flag's canonical spelling, derived from the message spelling rather
/// than restated from the parser's own table.
fn canonical_names() -> Vec<(String, Flag)> {
    Flag::ALL
        .iter()
        .map(|&flag| (normalize(&flag.display()[2..]), flag))
        .collect()
}

proptest! {
    /// Whatever the shell hands us, parsing produces an answer rather than a
    /// panic. Argv is attacker-adjacent and already lossily decoded (R55), so
    /// there is no input shape this may refuse to consider.
    #[test]
    fn parse_never_panics(argv in prop::collection::vec(token(), 0..8)) {
        let _ = cli::parse(&argv);
    }

    /// The slot-type invariant, which is what makes the text accessor total and
    /// what retires the two unreachable TypeScript messages \[C18\].
    #[test]
    fn slot_types_agree_with_arity(argv in prop::collection::vec(token(), 0..8)) {
        if let Ok(args) = cli::parse(&argv) {
            for (flag, value) in args.iter() {
                match value {
                    Value::Text(_) => prop_assert!(
                        flag.takes_value(), "{} is a switch holding text", flag.display()
                    ),
                    Value::Bool(_) | Value::Poison => prop_assert!(
                        !flag.takes_value(), "{} takes a value but holds a switch", flag.display()
                    ),
                }
            }
        }
    }

    /// Separators and case are noise: any sprinkling of `-`/`_` and any casing
    /// of a canonical name resolves to the same flag \[R41\].
    #[test]
    fn separators_and_case_round_trip(
        index in 0usize..Flag::COUNT,
        seeds in prop::collection::vec((any::<bool>(), any::<u8>()), 0..24),
    ) {
        let (name, flag) = canonical_names()[index].clone();

        let mut spelled = String::new();
        for (position, ch) in name.chars().enumerate() {
            if let Some(&(upper, separator)) = seeds.get(position) {
                if separator % 3 == 0 {
                    spelled.push('-');
                } else if separator % 3 == 1 {
                    spelled.push('_');
                }
                spelled.push(if upper { ch.to_ascii_uppercase() } else { ch });
            } else {
                spelled.push(ch);
            }
        }

        // Resolved against the extended table so route-only flags are covered
        // too: the property under test is that *normalisation* survives
        // separators and case, which is table-independent.
        prop_assert_eq!(
            Flag::resolve_in(&normalize(&spelled), Table::Extended),
            Some(flag),
            "{}",
            spelled
        );
    }

    /// A KELVIN SIGN is a `k` after full-Unicode lowercasing, and an
    /// `to_ascii_lowercase` port would fail exactly here \[R41\].
    #[test]
    fn kelvin_sign_resolves(index in 0usize..Flag::COUNT) {
        let (name, flag) = canonical_names()[index].clone();
        let spelled = name.replace('k', "\u{212A}");
        prop_assert_eq!(Flag::resolve_in(&normalize(&spelled), Table::Extended), Some(flag));
    }

    /// A bare switch consumes exactly two tokens when the next one is a boolean
    /// literal and exactly one otherwise \[R39\].
    #[test]
    fn switch_consumption_arity(next in token()) {
        // A `--` prefix would be parsed as the next option instead, and a bare
        // `-h` sets a flag rather than becoming a positional; neither is a
        // question about consumption.
        prop_assume!(!next.starts_with('-'));
        prop_assume!(!next.is_empty());

        let argv = vec!["market".to_owned(), "--detail".to_owned(), next.clone()];
        let args = cli::parse(&argv).expect("a switch followed by a bare word always parses");

        let consumed = args.positionals.is_empty();
        prop_assert_eq!(consumed, boolean_literal(&next).is_some(), "{}", next);
    }
}
