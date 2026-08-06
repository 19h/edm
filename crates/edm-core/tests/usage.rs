//! The usage text, against the original's.
//!
//! `edm help` is the most-read output the program has, it is 89 lines of
//! hand-aligned text, and several of its lines interpolate constants — the
//! endpoint paths and verbs, the concurrency and timeout defaults, the EDDN
//! upload URL and software name. A drift in any of those is both invisible in
//! review and a lie to the reader.
//!
//! `tests/fixtures/usage.txt` is `bun market-request.ts help` captured verbatim.

use edm_core::cli;

#[test]
fn the_usage_text_matches_the_original() {
    // `console.log` adds the trailing newline that the captured file has.
    // C34: the software name changed, so the line stating its default changed
    // with it — and the new value is not the same width, so the whole line
    // shifts. Masking that one line and comparing the other eighty-eight keeps
    // the pin where it is load-bearing. A help text documenting a default the
    // program does not use would be a worse thing for a reader than a masked
    // line is for a test.
    let mask = |text: &str| -> String {
        text.lines()
            .map(|line| {
                // The exact indented prefix: `str::trim_start` is banned in this
                // workspace \[R25\], and the alignment here is fixed anyway.
                if line.starts_with("  --software-name <n>") {
                    "<SOFTWARE-NAME LINE>"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let expected = mask(include_str!("fixtures/usage.txt"));
    let actual = mask(&format!("{}\n", cli::usage()));

    if actual != expected {
        // A whole-string assertion on 5.6 KB of aligned text is unreadable, so
        // report the first divergence with its neighbourhood instead.
        let mismatch = actual
            .lines()
            .zip(expected.lines())
            .enumerate()
            .find(|(_, (ours, theirs))| ours != theirs);

        if let Some((index, (ours, theirs))) = mismatch {
            panic!(
                "usage text differs at line {}:\n  bun:  {theirs:?}\n  rust: {ours:?}",
                index + 1
            );
        }
        panic!(
            "usage text has {} lines, the original has {}",
            actual.lines().count(),
            expected.lines().count()
        );
    }
}
