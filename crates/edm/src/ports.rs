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

    /// A `[0, 1)` sample for backoff jitter.
    ///
    /// Drawn from the same source as the nonce rather than from a new port, so
    /// a run pinned for reproducibility stays pinned in both respects.
    fn jitter_unit(&self) -> f64 {
        let bytes = self.nonce_bytes();
        let mut value = 0u64;
        for byte in bytes {
            value = (value << 8) | u64::from(byte);
        }
        // 48 bits over 2^48: uniform, and exactly representable in an f64.
        value as f64 / f64::from(1u32 << 24) / f64::from(1u32 << 24)
    }
}

/// The only filesystem write the program makes is `markets --dump`.
pub trait Fs {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()>;
    fn read_to_string(&self, path: &Path) -> std::io::Result<String>;
    fn create_dir_all(&self, path: &Path) -> std::io::Result<()>;
    fn exists(&self, path: &Path) -> bool;
}

/// Waiting, behind a seam.
///
/// The pacing policy is pure arithmetic that says *when* the next request may
/// go; this is the part that actually waits. Separating them is what lets a
/// test assert the **sequence of delays** a scenario produces rather than
/// sitting through them, which is both faster and a stronger statement than a
/// wall-clock measurement.
#[allow(async_fn_in_trait, reason = "single-threaded runtime; see HttpTransport")]
pub trait Timer {
    async fn sleep_ms(&self, millis: f64);
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

/// Real sleeping.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealTimer;

impl Timer for RealTimer {
    async fn sleep_ms(&self, millis: f64) {
        if millis > 0.0 {
            tokio::time::sleep(std::time::Duration::from_secs_f64(millis / 1000.0)).await;
        }
    }
}

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

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn create_dir_all(&self, path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Entropy with its jitter fraction pinned, and its nonces untouched.
///
/// `EDM_JITTER=0` \[C29\]. Backoff jitter is the one random quantity a recorded
/// run cannot reproduce — a nonce is already overridable through `--nonce`,
/// but the delay a retry waits is not, and it decides how many attempts fit
/// inside a wall-clock budget. Pinning it makes a retry scenario's *attempt
/// count* deterministic, which is the thing such a scenario exists to assert.
///
/// It does not touch `nonce_bytes`: a pinned nonce is a separate decision with
/// a separate flag, and folding them together would let one scenario's choice
/// silently reuse a keystream.
#[derive(Clone, Copy, Debug)]
pub struct PinnedJitter<'a, E> {
    pub inner: &'a E,
    pub unit: f64,
}

impl<E: Entropy> Entropy for PinnedJitter<'_, E> {
    fn nonce_bytes(&self) -> [u8; 6] {
        self.inner.nonce_bytes()
    }

    fn jitter_unit(&self) -> f64 {
        self.unit
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

/// An in-memory filesystem.
///
/// Read-write rather than write-only, because the cache's whole point is that a
/// second run sees the first run's writes — a recorder that only remembers
/// would make the resume path untestable.
#[derive(Debug, Default)]
pub struct RecordingFs(pub std::cell::RefCell<Vec<(std::path::PathBuf, String)>>);

impl RecordingFs {
    fn find(&self, path: &Path) -> Option<String> {
        self.0.borrow().iter().rev().find(|(at, _)| at == path).map(|(_, body)| body.clone())
    }
}

impl Fs for RecordingFs {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        self.0.borrow_mut().push((path.to_path_buf(), contents.to_owned()));
        Ok(())
    }

    fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
        self.find(path).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, path.display().to_string())
        })
    }

    fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.find(path).is_some()
    }
}

/// A timer that records what it was asked to wait for and returns at once.
#[derive(Debug, Default)]
pub struct RecordingTimer(pub std::cell::RefCell<Vec<f64>>);

impl RecordingTimer {
    #[must_use]
    pub fn delays(&self) -> Vec<f64> {
        self.0.borrow().clone()
    }
}

impl Timer for RecordingTimer {
    async fn sleep_ms(&self, millis: f64) {
        self.0.borrow_mut().push(millis);
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
