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

/// Where output actually goes.
///
/// An enum rather than a generic parameter because `Out` is threaded through
/// every command function and every one of them would otherwise grow a type
/// parameter that exists solely for tests. The branch is one predictable
/// compare per write against a 64 KiB buffer.
enum Sink {
    Stdout(BufWriter<std::io::Stdout>),
    /// Every write handed to a callback instead of a descriptor, tagged with
    /// the stream it would have gone to. Not `cfg(test)`: `edm ui` runs the
    /// whole pipeline behind one of these, because in raw mode the terminal is
    /// the screen and nothing may write to it behind the renderer's back
    /// \[C53\].
    Forward(ForwardSink),
    /// Both streams into one buffer, in write order — which is exactly what
    /// `2>&1` produces, and the ordering is itself part of what is under test.
    #[cfg(test)]
    Captured(String),
}

/// Where a forwarding sink hands each write.
pub type ForwardSink = Box<dyn Fn(Stream, &str)>;

/// Which descriptor a forwarded write belonged to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Sink {
    fn push(&mut self, stream: Stream, text: &str) {
        match self {
            Self::Stdout(writer) => {
                let _ = writer.write_all(text.as_bytes());
                let _ = writer.flush();
            }
            Self::Forward(sink) => sink(stream, text),
            #[cfg(test)]
            Self::Captured(buffer) => buffer.push_str(text),
        }
    }
}

/// The program's stdout, stderr and exit code.
pub struct Out {
    stdout: RefCell<Sink>,
    exit: Cell<u8>,
    width: usize,
    metric: Metric,
    /// `--json`: suppresses the tables, though not — faithfully — every
    /// diagnostic. R76.
    json: bool,
    /// Whether stdout is **one document** and nothing else may go there.
    ///
    /// R76's leak — a diagnostic landing in the middle of a `--json` stream —
    /// is faithful to the original and is reproduced for the four ported
    /// commands. `route` has no oracle to be faithful to, and a document a
    /// consumer cannot parse is not an output format \[C28\]. Set by `route
    /// --json`, it diverts every ordinary write to stderr, including the ones
    /// that come from emitters shared with the ported commands — which is where
    /// the leak actually came from: a single 410 error payload, and at region
    /// scale there is always one.
    documentary: Cell<bool>,
}

impl Out {
    #[must_use]
    pub fn new(width: usize, metric: Metric, json: bool) -> Self {
        Self {
            // 64 KiB: a commodity table at width 200 is a few kilobytes, and a
            // sweep emits one per market.
            stdout: RefCell::new(Sink::Stdout(BufWriter::with_capacity(
                64 * 1024,
                std::io::stdout(),
            ))),
            exit: Cell::new(0),
            width,
            metric,
            json,
            documentary: Cell::new(false),
        }
    }

    /// An `Out` whose every write is handed to `sink` with the stream it was
    /// bound for, so a full-screen UI can show the pipeline's own words
    /// without the pipeline knowing it is not on a console \[C53\].
    ///
    /// Everything else is unchanged: `--json`'s document mode still diverts
    /// ordinary writes to the stderr side, and the exit code still accumulates
    /// — it is simply never returned from the process.
    #[must_use]
    pub fn forwarding(width: usize, metric: Metric, sink: ForwardSink) -> Self {
        Self {
            stdout: RefCell::new(Sink::Forward(sink)),
            exit: Cell::new(0),
            width,
            metric,
            json: false,
            documentary: Cell::new(false),
        }
    }

    /// Declare that stdout carries one document and nothing else \[C28\].
    pub fn stdout_is_a_document(&self) {
        self.documentary.set(true);
    }

    /// Write the document itself. The one thing that reaches stdout in that
    /// mode, and the reason the mode exists.
    pub fn document(&self, text: &str) {
        if let Ok(mut sink) = self.stdout.try_borrow_mut() {
            sink.push(Stream::Stdout, text);
            sink.push(Stream::Stdout, "\n");
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

    /// Blocks that are *about* the run rather than its answer.
    ///
    /// Under `--json` they go to stderr, because stdout is then one document
    /// and a table in the middle of it corrupts the stream \[C28\]. Without
    /// `--json` they are ordinary output. Note this is deliberately **not** how
    /// the ported commands behave: R76's leaked diagnostics are faithful to the
    /// original and are reproduced, and this method is used only by `route`,
    /// which has no oracle to be faithful to.
    pub fn aside(&self, blocks: &[Block<'_>]) {
        let mut text = String::new();
        write_blocks(&mut text, blocks, self.width, self.metric);
        if self.json {
            self.diagnostic(&text);
        } else {
            self.write(&text);
        }
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
        self.diagnostic(&format!("{text}\n"));
    }

    /// A parse-error message, which the original follows with a blank line
    /// (`console.error(msg + "\n")`). R49.
    pub fn error_paragraph(&self, text: &str) {
        self.diagnostic(&format!("{text}\n\n"));
    }

    /// stderr, after stdout has been flushed so the two interleave the way the
    /// original's do.
    fn diagnostic(&self, text: &str) {
        self.flush();
        if let Ok(mut sink) = self.stdout.try_borrow_mut() {
            let forwarded = match &*sink {
                Sink::Stdout(_) => false,
                Sink::Forward(_) => true,
                #[cfg(test)]
                Sink::Captured(_) => true,
            };
            if forwarded {
                sink.push(Stream::Stderr, text);
                return;
            }
        }
        let _ = write!(std::io::stderr(), "{text}");
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
        if let Ok(mut sink) = self.stdout.try_borrow_mut()
            && let Sink::Stdout(writer) = &mut *sink
        {
            let _ = writer.flush();
        }
    }

    fn write(&self, text: &str) {
        if self.documentary.get() {
            self.diagnostic(text);
            return;
        }
        if let Ok(mut sink) = self.stdout.try_borrow_mut() {
            sink.push(Stream::Stdout, text);
        }
    }

    /// The exit code as assigned so far, for a caller that owns the process
    /// status itself rather than returning this `Out`'s.
    #[must_use]
    pub fn exit_status(&self) -> u8 {
        self.exit.get()
    }
}

#[cfg(test)]
impl Out {
    /// An `Out` that keeps everything, for asserting on what a command printed.
    #[must_use]
    pub fn capturing(width: usize, metric: Metric, json: bool) -> Self {
        Self {
            stdout: RefCell::new(Sink::Captured(String::new())),
            exit: Cell::new(0),
            width,
            metric,
            json,
            documentary: Cell::new(false),
        }
    }

    /// Everything written so far, stdout and stderr interleaved in write order.
    #[must_use]
    pub fn captured(&self) -> String {
        match &*self.stdout.borrow() {
            Sink::Stdout(_) | Sink::Forward(_) => String::new(),
            Sink::Captured(buffer) => buffer.clone(),
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
        assert_eq!(
            format!("{:?}", out.exit_code()),
            format!("{:?}", std::process::ExitCode::SUCCESS)
        );
        out.set_exit(EXIT_FAILURE);
        out.set_exit(EXIT_FAILURE);
        assert_eq!(out.exit.get(), 1);
        // A later usage error genuinely does overwrite it; the original assigns
        // rather than accumulates.
        out.set_exit(EXIT_USAGE);
        assert_eq!(out.exit.get(), 2);
    }

    /// A forwarding sink sees every write with the stream it was bound for,
    /// including the document-mode diversion, and nothing reaches a descriptor.
    #[test]
    fn a_forwarding_sink_tags_each_write_with_its_stream() {
        let seen = std::rc::Rc::new(RefCell::new(Vec::<(Stream, String)>::new()));
        let log = seen.clone();
        let out = Out::forwarding(
            40,
            Metric::Utf16,
            Box::new(move |stream, text| log.borrow_mut().push((stream, text.to_owned()))),
        );
        out.line("hello");
        out.error("oops");
        out.stdout_is_a_document();
        out.line("diverted");
        out.document("{}");
        out.set_exit(EXIT_FAILURE);
        assert_eq!(
            *seen.borrow(),
            vec![
                (Stream::Stdout, "hello\n".to_owned()),
                (Stream::Stderr, "oops\n".to_owned()),
                (Stream::Stderr, "diverted\n".to_owned()),
                (Stream::Stdout, "{}".to_owned()),
                (Stream::Stdout, "\n".to_owned()),
            ]
        );
        assert_eq!(out.exit_status(), EXIT_FAILURE);
    }
}
