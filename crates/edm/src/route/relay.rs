//! What has already been sent to EDDN, and when.
//!
//! A region sweep reads thousands of markets, and running it twice in an
//! afternoon would relay every one of them twice. EDDN is a shared firehose
//! that other people's tools consume: duplicate listings cost every downstream
//! consumer the same parse, and they are not merely wasteful — a second copy of
//! an unchanged listing carries a *newer* timestamp, so it looks like fresh
//! confirmation of a price nobody re-read.
//!
//! **Two rules, and the second is the one that matters.**
//!
//! 1. A market relayed within [`RouteConfig::eddn_max_age_minutes`] is not
//!    relayed again.
//! 2. **A listing that came from this program's own price cache is never
//!    relayed at all**, at any age. It was read at some earlier instant, and
//!    republishing it would stamp that old reading with the current time —
//!    which is exactly the lie rule 1 exists to prevent, told worse. Only a
//!    market polled live in this run has anything to say.
//!
//! One file per market, same shape and directory discipline as
//! [`crate::route::cache`]: a sweep killed half way keeps what it relayed, and
//! two runs over overlapping regions do not re-relay the overlap.

use std::path::{Path, PathBuf};

use edm_core::js;
use edm_core::js::json::{JsObject, JsValue};

use crate::ports::Fs;

/// Bumped when the stored shape changes; an older entry reads as absent.
const FORMAT_VERSION: u32 = 1;

/// How many refusals in a row before a run stops relaying.
///
/// **Relaying is an outward-facing side effect on somebody else's free
/// service.** When the gateway starts refusing, continuing to send is both
/// pointless and rude — and the way it refuses is a 403 from a proxy, which is
/// what happens to a host that has just sent hundreds of messages in a burst.
/// Five is enough to rule out a single bad message and few enough that a
/// blocked host stops almost immediately.
pub const GIVE_UP_AFTER: usize = 5;

/// The record of what this machine has relayed.
#[derive(Clone, Debug)]
pub struct Relayed {
    root: PathBuf,
    window_ms: f64,
}

/// What a run relayed, and what it held back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub sent: usize,
    pub failed: usize,
    /// What the gateway said the first time it refused.
    ///
    /// **Five hundred and one rejections once reported no reason at all.** The
    /// answer was `403 Forbidden` — a proxy refusing the host outright, not a
    /// schema complaint — and nothing in the output said so, which turned a
    /// one-line diagnosis into an investigation.
    pub first_refusal: Option<String>,
    /// Not attempted, because the gateway had already refused enough times to
    /// stop. See [`GIVE_UP_AFTER`].
    pub abandoned: usize,
    /// Suppressed because this machine relayed the same market recently.
    pub recent: usize,
    /// Suppressed because the listing came from the price cache, so there was
    /// nothing new to say about it.
    pub cached: usize,
    /// Suppressed because Ardent could not name the station, and EDDN's schema
    /// requires a system and station name.
    pub unnamed: usize,
}

impl Relayed {
    /// Beside the price cache, not inside it: the two answer different
    /// questions about the same market and expire on different clocks.
    #[must_use]
    pub fn new(cache_root: &Path, window_minutes: f64) -> Self {
        Self {
            root: cache_root.join("eddn"),
            window_ms: window_minutes * 60_000.0,
        }
    }

    fn path(&self, market_id: f64) -> PathBuf {
        // `js_number`, so the name is the decimal text the rest of the program
        // uses. A market recorded by one spelling and looked up by another is a
        // permanent miss, which here means permanent duplicate relaying.
        self.root.join(format!("{}.json", js::js_number(market_id)))
    }

    /// Whether this market may be relayed now.
    pub fn may_relay<F: Fs>(&self, fs: &F, market_id: f64, now_ms: f64) -> bool {
        let Ok(text) = fs.read_to_string(&self.path(market_id)) else {
            return true;
        };
        // Unreadable, truncated, or from an older format: relay it. The failure
        // direction here costs one duplicate message; the other direction would
        // silently stop relaying a market forever.
        let Some(at) = decode(&text) else { return true };
        now_ms - at >= self.window_ms
    }

    /// Note that it has been relayed. Failures are silent — a log that cannot
    /// be written costs a duplicate later, not this run.
    pub fn record<F: Fs>(&self, fs: &F, market_id: f64, now_ms: f64) {
        if fs.create_dir_all(&self.root).is_err() {
            return;
        }
        let entry = JsObject::from_document_order(vec![
            ("marketId".into(), JsValue::Num(market_id)),
            ("relayedAt".into(), JsValue::Num(now_ms)),
            ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
        ]);
        let _ = fs.write(
            &self.path(market_id),
            &JsValue::Obj(entry).stringify_compact(),
        );
    }
}

/// The instant a stored entry records, or `None` if it says nothing usable.
fn decode(text: &str) -> Option<f64> {
    let JsValue::Obj(object) = JsValue::parse(text).ok()? else {
        return None;
    };
    if object.get("version") != Some(&JsValue::Num(f64::from(FORMAT_VERSION))) {
        return None;
    }
    let JsValue::Num(at) = *object.get("relayedAt")? else {
        return None;
    };
    // A non-finite instant would make `now - at >= window` false forever and
    // suppress this market permanently.
    at.is_finite().then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingFs;

    const MINUTE: f64 = 60_000.0;

    fn log() -> Relayed {
        Relayed::new(Path::new("/cache"), 30.0)
    }

    #[test]
    fn a_market_never_relayed_may_be_relayed() {
        assert!(log().may_relay(&RecordingFs::default(), 1.0, 0.0));
    }

    #[test]
    fn a_market_relayed_inside_the_window_may_not() {
        let fs = RecordingFs::default();
        let log = log();
        log.record(&fs, 1.0, 0.0);

        assert!(!log.may_relay(&fs, 1.0, 29.0 * MINUTE));
        assert!(
            log.may_relay(&fs, 1.0, 30.0 * MINUTE),
            "and the window is inclusive at its edge"
        );
    }

    /// One market's record says nothing about another's.
    #[test]
    fn the_window_is_per_market() {
        let fs = RecordingFs::default();
        let log = log();
        log.record(&fs, 1.0, 0.0);

        assert!(!log.may_relay(&fs, 1.0, MINUTE));
        assert!(log.may_relay(&fs, 2.0, MINUTE));
    }

    /// A corrupt or truncated record relays. One duplicate message is the cheap
    /// failure; the other direction would silently stop relaying a market for
    /// good.
    #[test]
    fn an_unreadable_record_fails_towards_relaying() {
        let fs = RecordingFs::default();
        let log = log();
        fs.write(&log.path(9.0), "{\"marketId\":9,\"relayed")
            .expect("in memory");

        assert!(log.may_relay(&fs, 9.0, 0.0));
    }

    /// A non-finite instant would make every comparison false and suppress the
    /// market for ever.
    #[test]
    fn a_nonfinite_instant_does_not_suppress_forever() {
        let fs = RecordingFs::default();
        let log = log();
        fs.write(
            &log.path(9.0),
            "{\"marketId\":9,\"relayedAt\":null,\"version\":1}",
        )
        .expect("in memory");

        assert!(log.may_relay(&fs, 9.0, 1e12));
    }

    /// Beside the price cache, not inside it — the two expire on different
    /// clocks and a stray `.json` in the price directory would be parsed as a
    /// listing.
    #[test]
    fn the_log_lives_in_its_own_directory() {
        assert!(log().path(7.0).ends_with("eddn/7.json"));
    }

    /// The file name is the id as the program prints it. `4306502403.json`,
    /// never `4306502403.0.json`, or every lookup misses and every market is
    /// relayed on every run.
    #[test]
    fn the_file_name_is_the_id_the_program_prints() {
        assert!(log().path(4_306_502_403.0).ends_with("4306502403.json"));
    }
}
