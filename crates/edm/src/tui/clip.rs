//! Copy to the clipboard through the terminal \[C53\].
//!
//! OSC 52 asks the terminal emulator to set the clipboard; there is no
//! process-side clipboard here and no library for one. Terminals that do not
//! honour it drop the sequence silently, which is why the copied text is also
//! shown on screen and the status line says "sent", never "copied".

use std::io::Write;

use base64::Engine as _;

pub(crate) fn copy(text: &str) -> bool {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07").is_ok() && stdout.flush().is_ok()
}
