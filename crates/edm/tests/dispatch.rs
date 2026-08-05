//! `main`'s three tests, in the order they decide things (ts:3134).
//!
//! One `#[test]`; see `support/mod.rs` for why.

mod support;

use edm_core::cli;
use support::{FakeHttp, drive};

#[test]
fn main_dispatches_help_then_unknown_commands_then_errors() {
    let usage = format!("{}\n", cli::usage());

    // The `help` command.
    let run = drive(&["help"], &FakeHttp::default());
    run.assert_exit(0);
    assert_eq!(run.stdout, usage);
    assert!(run.stderr.is_empty());

    // R48: the `help` command is tested first, then the `--help` switch, and
    // both before the known-command set — so an unknown command carrying
    // `--help` gets the help text and exit **0**.
    let run = drive(&["bogus", "--help"], &FakeHttp::default());
    run.assert_exit(0);
    assert_eq!(run.stdout, usage);
    assert!(run.stderr.is_empty());

    // R49: the diagnostic and a **blank line** on stderr, `USAGE` on stdout,
    // exit 2.
    let run = drive(&["bogus"], &FakeHttp::default());
    run.assert_exit(2);
    assert_eq!(run.stderr, "Unknown command \"bogus\"\n\n");
    assert_eq!(run.stdout, usage);

    // A parse error takes the same two streams and the same exit code.
    let run = drive(&["market", "--nope"], &FakeHttp::default());
    run.assert_exit(2);
    assert_eq!(run.stderr, "Unknown option --nope\n\n");
    assert_eq!(run.stdout, usage);
    assert!(run.calls.is_empty(), "a parse error sends nothing");

    // R45: an empty token does not fill the command slot, so the next bare word
    // becomes the command.
    let run = drive(&["", "Colonia"], &FakeHttp::default());
    run.assert_exit(2);
    assert_eq!(run.stderr, "Unknown command \"colonia\"\n\n");

    // R82: an accessor failure prints `error.message` **alone** — no cause
    // chain, no usage text — and exits 1.
    let run = drive(&["market", "--market-id", "abc"], &FakeHttp::default());
    run.assert_exit(1);
    assert_eq!(run.stderr, "--market-id must be an unsigned decimal integer\n");
    assert!(run.stdout.is_empty());

    // R47: a switch that swallowed `constructor` holds a function, so
    // `optionalSwitch` throws when the session reads it — exit **1**, not the
    // exit 2 a naive port would give by leaving the token a positional. The
    // token really was consumed, so there is no positional either.
    let run = drive(&["market", "--json", "constructor"], &FakeHttp::default());
    run.assert_exit(1);
    assert_eq!(run.stderr, format!("{}\n", cli::POISON_TYPE_ERROR));
    assert!(run.stdout.is_empty(), "the poison is not a JSON run");

    // R50: every command loads the credentials, and the four fields are
    // validated in source order.
    let run = support::drive_with_env(
        &["markets", "Colonia", "--dry-run"],
        &FakeHttp::default(),
        vec![("AUTH_TOKEN".to_owned(), "short".to_owned())],
    );
    run.assert_exit(1);
    assert_eq!(run.stderr, "authToken must be exactly 2024 characters; received 5\n");
    assert!(run.calls.is_empty(), "credentials load before anything is sent");
}
