//! The command line's failure surface, against the original's.
//!
//! `tests/fixtures/cli_errors.tsv` records 41 invocations that fail before any
//! socket is opened, each with its exit code, its stdout and its stderr as the
//! original produced them. The parse-error subset is checkable here; the
//! accessor-error subset needs the command dispatch and is covered by
//! `cargo xtask parity`.
//!
//! Regenerate with `bun xtask/oracle/bless-cli-errors.ts crates/edm-core/tests/fixtures`.

use edm_core::cli::{self, POISON_TYPE_ERROR};

const FIXTURE: &str = include_str!("fixtures/cli_errors.tsv");

struct Case {
    name: String,
    argv: Vec<String>,
    exit: i32,
    stderr: String,
}

fn cases() -> Vec<Case> {
    FIXTURE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            Case {
                name: cols[0].to_owned(),
                argv: serde_json::from_str(cols[1]).expect("argv column"),
                exit: cols[2].parse().expect("exit column"),
                stderr: serde_json::from_str(cols[4]).expect("stderr column"),
            }
        })
        .collect()
}

/// Exit 2 means the *parser* refused, and the message is followed by a blank
/// line because the original writes `console.error(msg + "\n")`. R49.
#[test]
fn parse_errors_match_the_original() {
    let mut checked = 0;
    let mut failures = Vec::new();

    for case in cases().into_iter().filter(|c| c.exit == 2) {
        let expected = case.stderr.trim_end_matches('\n');
        match cli::parse(&case.argv) {
            Err(error) => {
                if error.to_string() != expected {
                    failures.push(format!(
                        "  {}\n    bun:  {expected:?}\n    rust: {:?}",
                        case.name,
                        error.to_string()
                    ));
                }
            }
            // Not every exit-2 case is a parse error: an unknown *command*
            // parses fine and is rejected by the dispatcher.
            Ok(args) => {
                let is_command_rejection =
                    expected.starts_with("Unknown command ") && !cli::is_known_command(&args.command);
                if !is_command_rejection {
                    failures.push(format!("  {}: parsed, but bun said {expected:?}", case.name));
                }
            }
        }
        checked += 1;
    }

    assert!(checked >= 10, "expected a meaningful parse-error corpus, got {checked}");
    assert!(failures.is_empty(), "{checked} cases:\n{}", failures.join("\n"));
}

/// `--help` and the `help` command both succeed, whatever else is on the line.
/// R48.
#[test]
fn help_always_wins() {
    for case in cases().into_iter().filter(|c| c.exit == 0) {
        let args = cli::parse(&case.argv)
            .unwrap_or_else(|e| panic!("{} should parse, got {e}", case.name));
        let wants_help = args.command == "help"
            || matches!(cli::Cli::new(&args, &cli::EnvSnapshot::empty())
                .switch_value(cli::Flag::Help, false), Ok(true));
        assert!(wants_help, "{} should have asked for help", case.name);
    }
}

/// R47: `BOOLEAN_LITERALS["constructor"]` resolves through `Object.prototype`,
/// so the token *is* consumed, the slot ends up holding a function, and the
/// later `value.toLowerCase()` throws — exit **1**, not the 2 a naive port
/// would produce by leaving the token as a positional.
///
/// The message is JavaScriptCore's, and it is measured rather than guessed.
#[test]
fn the_prototype_hit_carries_the_engine_s_own_message() {
    let case = cases()
        .into_iter()
        .find(|c| c.name == "poisoned_switch")
        .expect("the fixture carries a poisoned switch");

    assert_eq!(case.exit, 1, "a poisoned switch exits 1, not 2");
    assert_eq!(case.stderr.trim_end_matches('\n'), POISON_TYPE_ERROR);

    // And the token really is consumed: `constructor` must not survive as a
    // positional, which is what would have made this a usage error instead.
    let args = cli::parse(&case.argv).expect("it parses; the failure is a read");
    assert!(
        !args.positionals.iter().any(|p| p == "constructor"),
        "the literal was consumed by the switch, not left as a positional"
    );
}
