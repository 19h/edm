//! Everything the program prints, and the exit code it accumulates.
//!
//! Two behaviours here are inherited rather than chosen.
//!
//! **The exit code is assigned, not returned.** `process.exitCode = 1` marks the
//! run as failed and then carries on: a sweep whose third market 404s still
//! polls the other nine, prints its summary, and exits 1 at the end. Nothing in
//! this port may call `std::process::exit` — `clippy.toml` denies it.
//!
//! **Write failures are swallowed entirely.** `console.log` to a closed pipe
//! does not throw in Bun, so `edm market Colonia | head -5` exits cleanly rather
//! than with a broken-pipe error. Every write here returns `()`. R97.

use std::cell::{Cell, RefCell};
use std::io::{BufWriter, Write};

use edm_core::js::text::Metric;
use edm_core::render::{Block, write_blocks};

/// The exit code for a run that produced no usable data.
pub const EXIT_FAILURE: u8 = 1;
/// The exit code for a command line that could not be parsed.
pub const EXIT_USAGE: u8 = 2;

/// The program's stdout, stderr and exit code.
pub struct Out {
    stdout: RefCell<BufWriter<std::io::Stdout>>,
    exit: Cell<u8>,
    width: usize,
    metric: Metric,
    /// `--json`: suppresses the tables, though not — faithfully — every
    /// diagnostic. R76.
    json: bool,
}

impl Out {
    #[must_use]
    pub fn new(width: usize, metric: Metric, json: bool) -> Self {
        Self {
            // 64 KiB: a commodity table at width 200 is a few kilobytes, and a
            // sweep emits one per market.
            stdout: RefCell::new(BufWriter::with_capacity(64 * 1024, std::io::stdout())),
            exit: Cell::new(0),
            width,
            metric,
            json,
        }
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn metric(&self) -> Metric {
        self.metric
    }

    #[must_use]
    pub const fn is_json(&self) -> bool {
        self.json
    }

    /// Renders blocks to stdout.
    pub fn emit(&self, blocks: &[Block<'_>]) {
        let mut text = String::new();
        write_blocks(&mut text, blocks, self.width, self.metric);
        self.write(&text);
    }

    /// One already-complete line, plus its newline.
    pub fn line(&self, text: &str) {
        let mut owned = String::with_capacity(text.len() + 1);
        owned.push_str(text);
        owned.push('\n');
        self.write(&owned);
    }

    /// A streamed progress line, clamped to the terminal width. R33.
    pub fn progress(&self, text: &str) {
        self.emit(&[Block::Line(text.to_owned())]);
    }

    /// `console.error` — stderr, after stdout has been flushed.
    ///
    /// The flush is what keeps `edm ... 2>&1` readable: without it the buffered
    /// stdout would arrive after the unbuffered stderr and the two streams would
    /// interleave in an order the original never produces.
    pub fn error(&self, text: &str) {
        self.flush();
        let _ = writeln!(std::io::stderr(), "{text}");
    }

    /// A parse-error message, which the original follows with a blank line
    /// (`console.error(msg + "\n")`). R49.
    pub fn error_paragraph(&self, text: &str) {
        self.flush();
        let _ = writeln!(std::io::stderr(), "{text}\n");
    }

    /// Marks the run as failed without ending it.
    ///
    /// Last write wins and it is never reset, exactly like assigning
    /// `process.exitCode`. R75.
    pub fn set_exit(&self, code: u8) {
        self.exit.set(code);
    }

    #[must_use]
    pub fn exit_code(&self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.exit.get())
    }

    /// Pushes buffered output out.
    ///
    /// Called at every emission boundary rather than only at the end, so a
    /// `SIGINT` mid-sweep loses no more than the original would.
    pub fn flush(&self) {
        if let Ok(mut stdout) = self.stdout.try_borrow_mut() {
            let _ = stdout.flush();
        }
    }

    fn write(&self, text: &str) {
        if let Ok(mut stdout) = self.stdout.try_borrow_mut() {
            let _ = stdout.write_all(text.as_bytes());
            let _ = stdout.flush();
        }
    }
}

impl std::fmt::Debug for Out {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Out")
            .field("width", &self.width)
            .field("metric", &self.metric)
            .field("json", &self.json)
            .field("exit", &self.exit.get())
            .finish_non_exhaustive()
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exit_code_is_last_write_wins_and_never_resets() {
        let out = Out::new(100, Metric::Utf16, false);
        assert_eq!(format!("{:?}", out.exit_code()), format!("{:?}", std::process::ExitCode::SUCCESS));
        out.set_exit(EXIT_FAILURE);
        out.set_exit(EXIT_FAILURE);
        assert_eq!(out.exit.get(), 1);
        // A later usage error genuinely does overwrite it; the original assigns
        // rather than accumulates.
        out.set_exit(EXIT_USAGE);
        assert_eq!(out.exit.get(), 2);
    }
}
