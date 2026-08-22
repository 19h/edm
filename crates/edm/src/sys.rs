//! The handful of things that need the operating system.
//!
//! Isolated here so the rest of the binary is testable, and written against
//! `rustix` rather than `libc` so the whole workspace keeps `unsafe_code`
//! denied — a process that holds a 2024-character auth token in memory should
//! not also be the one hand-rolling `ioctl` calls.

use std::io::IsTerminal;

/// Makes the process a poor target for a memory dump.
///
/// Must be the first thing `main` does, before any credential is read. Two
/// steps, because either alone is insufficient: `RLIMIT_CORE` stops the kernel
/// writing a core file, and clearing the dumpable flag stops another process
/// with the same uid from attaching with `ptrace` and reading the tokens out of
/// our address space.
///
/// Failure is deliberately silent. This is defence in depth on a CLI tool, not
/// a precondition — a container that forbids `prctl` should still be able to
/// poll a market.
pub fn harden() {
    use rustix::process::{Resource, Rlimit};
    let _ = rustix::process::setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    );

    #[cfg(target_os = "linux")]
    {
        use rustix::process::DumpableBehavior;
        let _ = rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable);
    }
}

/// The terminal's width in columns, from the tty on **file descriptor 1**.
///
/// Descriptor 1 specifically, matching `process.stdout.columns`: a run whose
/// stdout is a pipe but whose stderr is still a terminal gets the fallback
/// width, not the terminal's. Returns `None` when stdout is not a terminal,
/// which is what makes `edm market Colonia | less` render at the default width
/// rather than at whatever happens to be attached elsewhere.
pub fn columns() -> Option<usize> {
    let stdout = std::io::stdout();
    if !stdout.is_terminal() {
        return None;
    }
    let size = rustix::termios::tcgetwinsize(&stdout).ok()?;
    (size.ws_col > 0).then_some(size.ws_col as usize)
}

/// Seconds since boot, as `os.uptime()` reports them.
///
/// Whole seconds, not fractional — which is why every `Request-Time` the
/// original sends is a multiple of 1000, and why
/// `request_time_is_a_multiple_of_1000` is a test rather than a coincidence.
pub fn uptime_seconds() -> f64 {
    rustix::system::sysinfo().uptime as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp the request header carries is `floor(uptime * 1000)`, and
    /// uptime is integral, so the low three digits are always zero. If this
    /// ever fails, `os.uptime()` has started reporting fractions and
    /// `Request-Time` needs re-checking against Bun.
    #[test]
    fn request_time_is_a_multiple_of_1000() {
        let stamp = (uptime_seconds() * 1000.0).floor();
        assert_eq!(
            stamp % 1000.0,
            0.0,
            "uptime is expected to be whole seconds"
        );
    }

    #[test]
    fn hardening_is_survivable() {
        // Not asserting the effect — a sandbox may refuse both calls — only
        // that it cannot take the process down.
        harden();
    }
}
