//! The journal, read on a thread of its own \[C53\].
//!
//! `commander::load_directory` reads every journal file whole, up to 64 MiB,
//! and does so synchronously. On the loop's thread that is a visible stall
//! every time the ship's position is wanted, so a reader thread owns the file
//! system and hands over a finished [`CommanderState`] instead. It re-reads on
//! an interval and on request, and warns once: the malformed-observation
//! warning is a property of the file, not of the read \[C49\].

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use crate::ports::RealFs;

use super::ThreadEvent;

/// What the loop can ask the reader for.
#[derive(Debug)]
pub(crate) enum JournalCmd {
    ReadNow,
    Stop,
}

/// A handle to the reader thread.
#[derive(Debug)]
pub(crate) struct JournalReader {
    cmd: mpsc::Sender<JournalCmd>,
}

impl JournalReader {
    pub(crate) fn read_now(&self) {
        let _ = self.cmd.send(JournalCmd::ReadNow);
    }

    pub(crate) fn stop(&self) {
        let _ = self.cmd.send(JournalCmd::Stop);
    }
}

/// Start reading `candidates` every `interval`, first read immediately.
///
/// The first directory that loads is the journal; the rest are the other
/// places it might have been. A read that fails everywhere sends nothing —
/// the screen shows "no journal" until one appears.
pub(crate) fn spawn(
    candidates: Vec<PathBuf>,
    interval: Duration,
    tx: async_channel::Sender<ThreadEvent>,
) -> Result<JournalReader, String> {
    let (cmd, commands) = mpsc::channel::<JournalCmd>();
    std::thread::Builder::new()
        .name("edm-journal".to_owned())
        .spawn(move || {
            let fs = RealFs;
            let mut warned = false;
            loop {
                let mut loaded = None;
                for directory in &candidates {
                    if !crate::ports::Fs::exists(&fs, directory) {
                        continue;
                    }
                    if let Ok(state) = crate::commander::load_directory(&fs, directory) {
                        loaded = Some(state);
                        break;
                    }
                }
                if let Some(mut state) = loaded {
                    // Said once. Sixty-eight repeats of the same warning would
                    // bury the log.
                    if warned {
                        state.warnings.clear();
                    } else {
                        warned = true;
                    }
                    if tx.send_blocking(ThreadEvent::Journal(Box::new(state))).is_err() {
                        break;
                    }
                }
                match commands.recv_timeout(interval) {
                    Ok(JournalCmd::ReadNow) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Ok(JournalCmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map_err(|error| format!("could not start the journal reader: {error}"))?;
    Ok(JournalReader { cmd })
}
