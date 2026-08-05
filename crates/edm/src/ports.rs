//! The ambient inputs, behind traits so tests can pin them.
//!
//! Three things make a run non-reproducible: the wall clock, the boot clock and
//! the entropy that seeds each request's nonce. The parity harness needs all
//! three fixed, and the original already exposes overrides for exactly them
//! (`--f-time`, `--request-time`, `--nonce`), so the seam is the program's own
//! rather than one invented for testing.
//!
//! These are static-dispatch traits with no `dyn` anywhere: the binary
//! monomorphises on the real implementations and pays nothing.

use std::path::Path;

/// Wall time and boot time.
pub trait Clock {
    /// Milliseconds since the Unix epoch, as `Date.now()` reports them.
    fn now_ms(&self) -> f64;

    /// Seconds since boot, as `os.uptime()` reports them — whole seconds.
    fn uptime_seconds(&self) -> f64;

    /// `Math.floor(Date.now() / 1000)`, the default `fTime`.
    fn frontier_time(&self) -> f64 {
        (self.now_ms() / 1000.0).floor()
    }
}

/// Where a request nonce comes from.
pub trait Entropy {
    /// Six bytes, which the caller renders as twelve lowercase hex characters.
    fn nonce_bytes(&self) -> [u8; 6];
}

/// The only filesystem write the program makes is `markets --dump`.
pub trait Fs {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()>;
}

// ---------------------------------------------------------------------------
// The real ones
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }

    fn uptime_seconds(&self) -> f64 {
        crate::sys::uptime_seconds()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn nonce_bytes(&self) -> [u8; 6] {
        let mut bytes = [0u8; 6];
        // A nonce that repeated would reuse a keystream. There is no sensible
        // fallback, so a failure here is fatal rather than quietly weak.
        getrandom::fill(&mut bytes).expect("the OS must provide entropy for a request nonce");
        bytes
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

impl Fs for RealFs {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        std::fs::write(path, contents)
    }
}

/// Everything ambient, in one place to thread through the command functions.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ports<C, E, F> {
    pub clock: C,
    pub entropy: E,
    pub fs: F,
}

impl Ports<SystemClock, OsEntropy, RealFs> {
    #[must_use]
    pub fn real() -> Self {
        Self { clock: SystemClock, entropy: OsEntropy, fs: RealFs }
    }
}

// ---------------------------------------------------------------------------
// The fixed ones
// ---------------------------------------------------------------------------

/// A clock that does not move, so a recorded run replays byte-for-byte.
#[derive(Clone, Copy, Debug)]
pub struct FixedClock {
    pub now_ms: f64,
    pub uptime_seconds: f64,
}

impl Clock for FixedClock {
    fn now_ms(&self) -> f64 {
        self.now_ms
    }

    fn uptime_seconds(&self) -> f64 {
        self.uptime_seconds
    }
}

/// Entropy that counts, so successive nonces in one run are distinct but
/// predictable — which matters because the original draws a nonce per request
/// and the parity harness compares the whole sequence.
#[derive(Debug, Default)]
pub struct CountingEntropy(std::cell::Cell<u16>);

impl Entropy for CountingEntropy {
    fn nonce_bytes(&self) -> [u8; 6] {
        let n = self.0.get();
        self.0.set(n.wrapping_add(1));
        let [hi, lo] = n.to_be_bytes();
        [0, 0, 0, 0, hi, lo]
    }
}

/// A filesystem that records rather than writes.
#[derive(Debug, Default)]
pub struct RecordingFs(pub std::cell::RefCell<Vec<(std::path::PathBuf, String)>>);

impl Fs for RecordingFs {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        self.0.borrow_mut().push((path.to_path_buf(), contents.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_time_truncates_to_whole_seconds() {
        let clock = FixedClock { now_ms: 1_700_000_000_999.0, uptime_seconds: 42.0 };
        assert_eq!(clock.frontier_time(), 1_700_000_000.0);
    }

    #[test]
    fn counting_entropy_never_repeats_within_a_run() {
        let entropy = CountingEntropy::default();
        let first = entropy.nonce_bytes();
        let second = entropy.nonce_bytes();
        assert_ne!(first, second);
    }
}
