//! Terminal rendering: the fitted table, headings and wrapped notes.
//!
//! Everything here is a pure function of its input and a terminal width, so the
//! complete output of any command is a value that can be snapshot-tested at any
//! width with no runtime, no transport and no terminal attached.
