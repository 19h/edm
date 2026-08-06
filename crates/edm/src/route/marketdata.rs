//! Batched candidate-price reads from `/starsystem/marketdata`.
//!
//! This endpoint is deliberately kept apart from `route::cache`: its rows are
//! candidate prices without stock or demand quantities, whereas `/market/list`
//! is quantity-aware.  A system is the cache unit because one five-system
//! response can give each system a different server expiry.

use std::collections::BTreeSet;
use std::fmt::Display;
use std::future::Future;
use std::path::{Path, PathBuf};

use edm_core::domain::{marketdata, resources::FinanceRules};
use edm_core::js::json::{JsObject, JsValue};

use crate::ports::Fs;

/// Observed client policy, enforced independently of the live finance value.
pub const MARKETDATA_BATCH_MAX: usize = 5;

const PROVIDER_NAMESPACE: &str = "frontier-marketdata";
const FORMAT_VERSION: u32 = 1;

/// On-disk policy for candidate system data.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    max_age_ms: f64,
    enabled: bool,
    refresh: bool,
}

impl Cache {
    /// Construct a cache using the route command's user-facing minute value.
    #[must_use]
    pub fn new(root: PathBuf, max_age_minutes: f64, enabled: bool, refresh: bool) -> Self {
        Self::with_max_age_ms(root, max_age_minutes * 60_000.0, enabled, refresh)
    }

    /// Millisecond constructor for callers that already converted the option.
    #[must_use]
    pub fn with_max_age_ms(root: PathBuf, max_age_ms: f64, enabled: bool, refresh: bool) -> Self {
        Self {
            root,
            max_age_ms,
            enabled,
            refresh,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One raw system per file.  IDs never pass through an `f64`.
    #[must_use]
    pub fn path(&self, address: u64) -> PathBuf {
        self.root
            .join(PROVIDER_NAMESPACE)
            .join(format!("system-{address}.json"))
    }

    /// Read one system, rejecting any envelope or payload that cannot prove it
    /// belongs to the requested address and this provider version.
    #[must_use]
    pub fn get<F: Fs>(&self, fs: &F, address: u64, now_ms: f64) -> Lookup {
        if !self.enabled || self.refresh {
            return Lookup::Skipped;
        }
        let Ok(text) = fs.read_to_string(&self.path(address)) else {
            return Lookup::Missing;
        };
        let Some(entry) = decode(&text, address) else {
            return Lookup::Corrupt;
        };

        let age_ms = now_ms - entry.read_at_ms;
        if !now_ms.is_finite() || !age_ms.is_finite() || age_ms < 0.0 {
            return Lookup::Corrupt;
        }
        if !self.max_age_ms.is_finite() || self.max_age_ms < 0.0 {
            return Lookup::Stale { age_ms };
        }
        if age_ms > self.max_age_ms || now_ms > entry.expires_at_ms {
            return Lookup::Stale { age_ms };
        }
        // The envelope's address is not enough.  When the raw object carries
        // `systemAddr`, `address`, or `id64`, the typed parser also proves that
        // embedded ID agrees before a cache hit is admitted.
        if typed_system(address, &entry.raw).is_none() {
            return Lookup::Corrupt;
        }
        Lookup::Fresh(entry)
    }

    /// Bank one successfully typed raw system.
    ///
    /// A future server `cacheuntil` wins.  Missing, invalid, or already expired
    /// server times use `SystemMarketCacheTime` relative to this read instead.
    /// The user's max age remains a read policy, rather than being baked into
    /// the file, so a later run may choose a stricter age.
    pub fn put<F: Fs>(
        &self,
        fs: &F,
        address: u64,
        raw: &JsValue,
        read_at_ms: f64,
        cache_until_s: i64,
        rules: FinanceRules,
    ) {
        if !self.enabled || !read_at_ms.is_finite() {
            return;
        }
        let expires_at_ms = expiry_ms(read_at_ms, cache_until_s, rules);
        if !expires_at_ms.is_finite() || expires_at_ms < read_at_ms {
            return;
        }

        let provider_root = self.root.join(PROVIDER_NAMESPACE);
        if fs.create_dir_all(&provider_root).is_err() {
            return;
        }
        let envelope = JsObject::from_document_order(vec![
            ("provider".into(), JsValue::Str(PROVIDER_NAMESPACE.into())),
            ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
            ("address".into(), JsValue::Str(address.to_string().into())),
            ("readAtMs".into(), JsValue::Num(read_at_ms)),
            ("expiresAtMs".into(), JsValue::Num(expires_at_ms)),
            ("raw".into(), raw.clone()),
        ]);
        let _ = fs.write(
            &self.path(address),
            &JsValue::Obj(envelope).stringify_compact(),
        );
    }
}

/// One valid cache envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub address: u64,
    pub read_at_ms: f64,
    pub expires_at_ms: f64,
    pub raw: JsValue,
}

/// Result of consulting one system cache file.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    Fresh(Entry),
    Stale {
        age_ms: f64,
    },
    Missing,
    Corrupt,
    /// `--refresh` and `--no-cache` do not consult the filesystem.
    Skipped,
}

/// Cache coverage for all requested systems.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hits {
    pub fresh: usize,
    pub stale: usize,
    pub missing: usize,
    pub corrupt: usize,
}

impl Lookup {
    fn tally(&self, hits: &mut Hits) {
        match self {
            Self::Fresh(_) => hits.fresh += 1,
            Self::Stale { .. } => hits.stale += 1,
            Self::Missing | Self::Skipped => hits.missing += 1,
            Self::Corrupt => hits.corrupt += 1,
        }
    }
}

/// A transport or whole-document failure.  Omitted or malformed individual
/// systems are instead listed in [`Acquired::missing`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedBatch {
    pub addresses: Vec<u64>,
    pub error: String,
}

/// Alternate spelling useful at call sites that describe failures first.
pub type BatchFailure = FailedBatch;

/// Candidate system data acquired from cache and live batches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Acquired {
    /// Typed candidate data.  These domain types contain no quantities and do
    /// not assert that a market has been verified.
    pub systems: Vec<marketdata::SystemMarketData>,
    /// Every requested address for which neither cache nor its batch supplied
    /// a valid typed system.
    pub missing: Vec<u64>,
    pub failed_batches: Vec<FailedBatch>,
    pub cache: Hits,
    /// Convenient scalar for progress/coverage summaries.
    pub cache_hits: usize,
}

/// Deduplicate, sort, and split exact addresses into deterministic batches.
///
/// A zero live setting cannot create zero-sized chunks; it degrades to one.
/// Values above five are capped even if a server happens to accept them.
#[must_use]
pub fn batches(addresses: &[u64], max: usize) -> Vec<Vec<u64>> {
    let size = max.clamp(1, MARKETDATA_BATCH_MAX);
    let unique = addresses
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    unique.chunks(size).map(<[u64]>::to_vec).collect()
}

/// Consult the per-system cache, then fetch only the misses.
///
/// `fetch_batch` is the HTTP boundary: it receives exact numeric IDs (never an
/// `f64`, and never a pre-built comma string), at most five at a time, and
/// returns the already decrypted response text.  Successful requested systems
/// are typed and cached one by one as soon as their batch lands; a partial
/// response therefore banks its useful four systems even when the fifth is
/// omitted.
pub async fn acquire<F, Fetch, Fut, E>(
    addresses: &[u64],
    cache: &Cache,
    fs: &F,
    now_ms: f64,
    rules: FinanceRules,
    mut fetch_batch: Fetch,
) -> Acquired
where
    F: Fs,
    Fetch: FnMut(Vec<u64>) -> Fut,
    Fut: Future<Output = Result<String, E>>,
    E: Display,
{
    let requested = addresses.iter().copied().collect::<BTreeSet<_>>();
    let mut acquired = Acquired::default();
    let mut landed = BTreeSet::new();
    let mut to_fetch = Vec::new();

    for address in requested.iter().copied() {
        let lookup = cache.get(fs, address, now_ms);
        match lookup {
            Lookup::Fresh(entry) => {
                // `get` has already validated this reconstruction.  Keep the
                // guard here so an invariant failure is a miss, never a panic.
                if let Some(system) = typed_system(address, &entry.raw) {
                    acquired.cache.fresh += 1;
                    landed.insert(address);
                    acquired.systems.push(system);
                } else {
                    acquired.cache.corrupt += 1;
                    to_fetch.push(address);
                }
            }
            other => {
                other.tally(&mut acquired.cache);
                to_fetch.push(address);
            }
        }
    }

    for batch in batches(&to_fetch, rules.systems_per_request) {
        let text = match fetch_batch(batch.clone()).await {
            Ok(text) => text,
            Err(error) => {
                acquired.failed_batches.push(FailedBatch {
                    addresses: batch,
                    error: error.to_string(),
                });
                continue;
            }
        };
        let document = match JsValue::parse(&text) {
            Ok(document) => document,
            Err(error) => {
                acquired.failed_batches.push(FailedBatch {
                    addresses: batch,
                    error: format!("invalid marketdata JSON: {error}"),
                });
                continue;
            }
        };
        let Some(raw_systems) = document
            .as_record()
            .and_then(|root| root.get("starsystems"))
            .and_then(JsValue::as_record)
        else {
            acquired.failed_batches.push(FailedBatch {
                addresses: batch,
                error: "marketdata response has no starsystems object".to_owned(),
            });
            continue;
        };

        let wanted = batch.iter().copied().collect::<BTreeSet<_>>();
        let parsed = marketdata::parse_marketdata(&document);
        for system in parsed.systems {
            let address = system.address;
            if !wanted.contains(&address) || landed.contains(&address) {
                continue;
            }
            let key = address.to_string();
            let Some(raw) = raw_systems.get(&key) else {
                continue;
            };
            // Cache the untouched individual system object, not a re-encoded
            // typed projection: future parsers may learn fields this version
            // does not yet know.
            cache.put(fs, address, raw, now_ms, system.cache_until_s, rules);
            landed.insert(address);
            acquired.systems.push(system);
        }
    }

    acquired.systems.sort_by_key(|system| system.address);
    acquired.missing = requested.difference(&landed).copied().collect();
    acquired.cache_hits = acquired.cache.fresh;
    acquired
}

fn decode(text: &str, expected_address: u64) -> Option<Entry> {
    let JsValue::Obj(object) = JsValue::parse(text).ok()? else {
        return None;
    };
    if object.get("provider").and_then(JsValue::as_str) != Some(PROVIDER_NAMESPACE) {
        return None;
    }
    if object.get("version") != Some(&JsValue::Num(f64::from(FORMAT_VERSION))) {
        return None;
    }
    let address = object
        .get("address")
        .and_then(edm_core::domain::resources::exact_u64)?;
    if address != expected_address {
        return None;
    }
    let read_at_ms = object.get("readAtMs").and_then(JsValue::as_f64)?;
    let expires_at_ms = object.get("expiresAtMs").and_then(JsValue::as_f64)?;
    if !read_at_ms.is_finite() || !expires_at_ms.is_finite() || expires_at_ms < read_at_ms {
        return None;
    }
    let raw = object.get("raw")?.clone();
    raw.as_record()?;
    Some(Entry {
        address,
        read_at_ms,
        expires_at_ms,
        raw,
    })
}

fn typed_system(address: u64, raw: &JsValue) -> Option<marketdata::SystemMarketData> {
    let document = JsValue::Obj(JsObject::from_document_order(vec![(
        "starsystems".into(),
        JsValue::Obj(JsObject::from_document_order(vec![(
            address.to_string().into(),
            raw.clone(),
        )])),
    )]));
    let mut parsed = marketdata::parse_marketdata(&document).systems;
    (parsed.len() == 1 && parsed[0].address == address).then(|| parsed.remove(0))
}

fn expiry_ms(read_at_ms: f64, cache_until_s: i64, rules: FinanceRules) -> f64 {
    let server_ms = cache_until_s as f64 * 1_000.0;
    if server_ms.is_finite() && server_ms > read_at_ms {
        server_ms
    } else {
        read_at_ms + rules.system_market_cache_seconds as f64 * 1_000.0
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::ports::RecordingFs;
    use edm_core::domain::marketdata::Side;

    const NOW_MS: f64 = 1_700_000_000_000.0;

    fn rules(systems_per_request: usize) -> FinanceRules {
        FinanceRules {
            system_market_cache_seconds: 7_200,
            max_marketdata_distance_ly: 40.0,
            systems_per_request,
        }
    }

    fn raw_system(address: u64, cache_until_s: i64) -> JsValue {
        JsValue::parse(&format!(
            r#"{{
                "systemAddr":"{address}",
                "name":"System {address}",
                "hasFleetCarriers":false,
                "techBroker":"none",
                "materialTrader":"none",
                "blackMarket":false,
                "facilitator":false,
                "voucherredemption":false,
                "carrierVendor":false,
                "modulepacks":false,
                "cacheuntil":{cache_until_s},
                "markets":{{}}
            }}"#,
        ))
        .expect("system fixture is JSON")
    }

    fn raw_candidate_system(address: u64, cache_until_s: i64) -> JsValue {
        JsValue::parse(&format!(
            r#"{{
                "systemAddr":"{address}",
                "name":"System {address}",
                "hasFleetCarriers":false,
                "techBroker":"none",
                "materialTrader":"none",
                "blackMarket":false,
                "facilitator":false,
                "voucherredemption":false,
                "carrierVendor":false,
                "modulepacks":false,
                "cacheuntil":{cache_until_s},
                "markets":{{
                    "1001":{{
                        "id":"1001",
                        "systemName":"System {address}",
                        "name":"Candidate Port",
                        "distFromSystem":12.5,
                        "market_state":"",
                        "starsystem_id":"77",
                        "service_blackmarket":"0",
                        "service_commodities":"1",
                        "allowDumping":false,
                        "simulatedAt":1699999999,
                        "smallPads":true,
                        "mediumPads":true,
                        "largePads":true,
                        "surface":false,
                        "commodities":{{
                            "10":{{
                                "type":"producer",
                                "buyPrice":100,
                                "sellPrice":90,
                                "illegal":false,
                                "illegalJurisdictionQty":31
                            }},
                            "20":{{
                                "type":"consumer",
                                "buyPrice":1,
                                "sellPrice":500,
                                "illegal":true,
                                "illegalJurisdictionQty":46
                            }}
                        }}
                    }}
                }}
            }}"#,
        ))
        .expect("candidate fixture is JSON")
    }

    fn response(rows: &[(u64, i64)]) -> String {
        let systems = rows
            .iter()
            .map(|(address, until)| (address.to_string().into(), raw_system(*address, *until)))
            .collect();
        JsValue::Obj(JsObject::from_document_order(vec![(
            "starsystems".into(),
            JsValue::Obj(JsObject::from_document_order(systems)),
        )]))
        .stringify_compact()
    }

    fn candidate_response(address: u64, until: i64) -> String {
        JsValue::Obj(JsObject::from_document_order(vec![(
            "starsystems".into(),
            JsValue::Obj(JsObject::from_document_order(vec![(
                address.to_string().into(),
                raw_candidate_system(address, until),
            )])),
        )]))
        .stringify_compact()
    }

    fn envelope(
        address: u64,
        read_at_ms: f64,
        expires_at_ms: f64,
        provider: &str,
        version: u32,
        raw: JsValue,
    ) -> String {
        JsValue::Obj(JsObject::from_document_order(vec![
            ("provider".into(), JsValue::Str(provider.into())),
            ("version".into(), JsValue::Num(f64::from(version))),
            ("address".into(), JsValue::Str(address.to_string().into())),
            ("readAtMs".into(), JsValue::Num(read_at_ms)),
            ("expiresAtMs".into(), JsValue::Num(expires_at_ms)),
            ("raw".into(), raw),
        ]))
        .stringify_compact()
    }

    #[test]
    fn deterministic_batches_cover_zero_one_five_six_and_eleven() {
        assert_eq!(batches(&[], 5), Vec::<Vec<u64>>::new());
        assert_eq!(batches(&[7], 5), vec![vec![7]]);
        assert_eq!(batches(&[5, 3, 1, 4, 2], 5), vec![vec![1, 2, 3, 4, 5]]);
        assert_eq!(
            batches(&[6, 5, 4, 3, 2, 1, 3], 5),
            vec![vec![1, 2, 3, 4, 5], vec![6]]
        );
        assert_eq!(
            batches(&(1..=11).rev().collect::<Vec<_>>(), usize::MAX),
            vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9, 10], vec![11]]
        );
        assert_eq!(batches(&[3, 2, 1], 0), vec![vec![1], vec![2], vec![3]]);
    }

    #[tokio::test]
    async fn acquire_uses_the_zero_one_five_six_and_eleven_batch_shapes() {
        for count in [0_u64, 1, 5, 6, 11] {
            let fs = RecordingFs::default();
            let cache = Cache::with_max_age_ms(
                PathBuf::from(format!("/cache/{count}")),
                86_400_000.0,
                true,
                false,
            );
            let addresses = (1..=count).rev().collect::<Vec<_>>();
            let calls = RefCell::new(Vec::new());
            let acquired = acquire(&addresses, &cache, &fs, NOW_MS, rules(99), |batch| {
                calls.borrow_mut().push(batch.clone());
                let rows = batch
                    .iter()
                    .map(|address| (*address, (NOW_MS / 1_000.0) as i64 + 3_600))
                    .collect::<Vec<_>>();
                std::future::ready(Ok::<_, String>(response(&rows)))
            })
            .await;

            assert_eq!(acquired.systems.len(), count as usize);
            assert!(acquired.missing.is_empty());
            assert!(acquired.failed_batches.is_empty());
            let expected = match count {
                0 => vec![],
                1 => vec![vec![1]],
                5 => vec![vec![1, 2, 3, 4, 5]],
                6 => vec![vec![1, 2, 3, 4, 5], vec![6]],
                11 => vec![vec![1, 2, 3, 4, 5], vec![6, 7, 8, 9, 10], vec![11]],
                _ => unreachable!(),
            };
            assert_eq!(*calls.borrow(), expected);
        }
    }

    #[tokio::test]
    async fn a_partial_four_of_five_response_is_retained_and_only_one_is_missing() {
        let fs = RecordingFs::default();
        let cache = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let addresses = [1, 2, 3, 4, 5];
        let until = (NOW_MS / 1_000.0) as i64 + 3_600;

        let first = acquire(&addresses, &cache, &fs, NOW_MS, rules(5), |batch| {
            assert_eq!(batch, vec![1, 2, 3, 4, 5]);
            std::future::ready(Ok::<_, String>(response(&[
                (1, until),
                (2, until),
                (3, until),
                (4, until),
            ])))
        })
        .await;

        assert_eq!(
            first
                .systems
                .iter()
                .map(|system| system.address)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(first.missing, vec![5]);
        assert!(
            first.failed_batches.is_empty(),
            "an omission is not a failed request"
        );
        assert_eq!(
            fs.0.borrow().len(),
            4,
            "each good system was banked immediately"
        );

        let calls = RefCell::new(Vec::new());
        let second = acquire(&addresses, &cache, &fs, NOW_MS + 1.0, rules(5), |batch| {
            calls.borrow_mut().push(batch.clone());
            std::future::ready(Ok::<_, String>(response(&[(5, until)])))
        })
        .await;
        assert_eq!(*calls.borrow(), vec![vec![5]]);
        assert_eq!(second.cache_hits, 4);
        assert_eq!(second.systems.len(), 5);
        assert!(second.missing.is_empty());
    }

    #[tokio::test]
    async fn each_system_uses_its_own_server_expiry() {
        let fs = RecordingFs::default();
        let cache = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let now_s = (NOW_MS / 1_000.0) as i64;
        let first = acquire(&[1, 2], &cache, &fs, NOW_MS, rules(5), |_| {
            std::future::ready(Ok::<_, String>(response(&[
                (1, now_s + 10),
                (2, now_s + 20),
            ])))
        })
        .await;
        assert_eq!(first.systems.len(), 2);

        let calls = RefCell::new(Vec::new());
        let second = acquire(&[1, 2], &cache, &fs, NOW_MS + 15_000.0, rules(5), |batch| {
            calls.borrow_mut().push(batch.clone());
            std::future::ready(Ok::<_, String>(response(&[(1, now_s + 30)])))
        })
        .await;

        assert_eq!(*calls.borrow(), vec![vec![1]]);
        assert_eq!(
            second.cache,
            Hits {
                fresh: 1,
                stale: 1,
                missing: 0,
                corrupt: 0
            }
        );
        assert_eq!(second.systems.len(), 2);
    }

    #[tokio::test]
    async fn expired_cacheuntil_falls_back_to_7200_but_user_age_can_be_stricter() {
        let fs = RecordingFs::default();
        let wide = Cache::with_max_age_ms(PathBuf::from("/cache"), 10_800_000.0, true, false);
        acquire(&[1], &wide, &fs, NOW_MS, rules(5), |_| {
            std::future::ready(Ok::<_, String>(response(&[(1, 0)])))
        })
        .await;

        assert!(matches!(
            wide.get(&fs, 1, NOW_MS + 7_200_000.0),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            wide.get(&fs, 1, NOW_MS + 7_200_001.0),
            Lookup::Stale { .. }
        ));

        let strict = Cache::with_max_age_ms(PathBuf::from("/cache"), 60_000.0, true, false);
        assert!(matches!(
            strict.get(&fs, 1, NOW_MS + 60_000.0),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            strict.get(&fs, 1, NOW_MS + 60_001.0),
            Lookup::Stale { .. }
        ));
    }

    #[tokio::test]
    async fn future_corrupt_wrong_provider_version_and_ids_are_all_misses() {
        let fs = RecordingFs::default();
        let cache = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let valid_until = (NOW_MS / 1_000.0) as i64 + 3_600;
        let expiry = NOW_MS + 3_600_000.0;

        fs.write(
            &cache.path(1),
            &envelope(
                1,
                NOW_MS + 1.0,
                expiry,
                PROVIDER_NAMESPACE,
                FORMAT_VERSION,
                raw_system(1, valid_until),
            ),
        )
        .unwrap();
        fs.write(&cache.path(2), "{not json").unwrap();
        fs.write(
            &cache.path(3),
            &envelope(
                3,
                NOW_MS,
                expiry,
                "another-source",
                FORMAT_VERSION,
                raw_system(3, valid_until),
            ),
        )
        .unwrap();
        fs.write(
            &cache.path(4),
            &envelope(
                4,
                NOW_MS,
                expiry,
                PROVIDER_NAMESPACE,
                FORMAT_VERSION + 1,
                raw_system(4, valid_until),
            ),
        )
        .unwrap();
        fs.write(
            &cache.path(5),
            &envelope(
                50,
                NOW_MS,
                expiry,
                PROVIDER_NAMESPACE,
                FORMAT_VERSION,
                raw_system(5, valid_until),
            ),
        )
        .unwrap();
        fs.write(
            &cache.path(6),
            &envelope(
                6,
                NOW_MS,
                expiry,
                PROVIDER_NAMESPACE,
                FORMAT_VERSION,
                raw_system(60, valid_until),
            ),
        )
        .unwrap();

        let calls = RefCell::new(Vec::new());
        let acquired = acquire(
            &[1, 2, 3, 4, 5, 6],
            &cache,
            &fs,
            NOW_MS,
            rules(5),
            |batch| {
                calls.borrow_mut().push(batch.clone());
                let rows = batch
                    .iter()
                    .map(|address| (*address, valid_until))
                    .collect::<Vec<_>>();
                std::future::ready(Ok::<_, String>(response(&rows)))
            },
        )
        .await;

        assert_eq!(*calls.borrow(), vec![vec![1, 2, 3, 4, 5], vec![6]]);
        assert_eq!(acquired.cache.corrupt, 6);
        assert_eq!(acquired.cache_hits, 0);
        assert!(acquired.missing.is_empty());
    }

    #[tokio::test]
    async fn refresh_reads_nothing_but_writes_while_no_cache_does_neither() {
        let fs = RecordingFs::default();
        let normal = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let until = (NOW_MS / 1_000.0) as i64 + 3_600;
        normal.put(&fs, 1, &raw_system(1, until), NOW_MS, until, rules(5));

        let refresh = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, true);
        let refresh_calls = RefCell::new(0);
        let refreshed = acquire(&[1], &refresh, &fs, NOW_MS + 1_000.0, rules(5), |batch| {
            *refresh_calls.borrow_mut() += 1;
            std::future::ready(Ok::<_, String>(response(&[(batch[0], until)])))
        })
        .await;
        assert_eq!(*refresh_calls.borrow(), 1);
        assert_eq!(refreshed.cache_hits, 0);
        let after_refresh = fs.0.borrow().len();
        assert_eq!(
            after_refresh, 2,
            "refresh replaced the entry with a new write"
        );

        let disabled = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, false, false);
        let disabled_result = acquire(&[1], &disabled, &fs, NOW_MS + 2_000.0, rules(5), |batch| {
            std::future::ready(Ok::<_, String>(response(&[(batch[0], until)])))
        })
        .await;
        assert_eq!(disabled_result.cache_hits, 0);
        assert_eq!(
            fs.0.borrow().len(),
            after_refresh,
            "--no-cache did not write"
        );
    }

    #[tokio::test]
    async fn candidate_sides_remain_candidate_only_and_never_gain_quantities() {
        let fs = RecordingFs::default();
        let cache = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let until = (NOW_MS / 1_000.0) as i64 + 3_600;
        let acquired = acquire(&[1], &cache, &fs, NOW_MS, rules(5), |_| {
            std::future::ready(Ok::<_, String>(candidate_response(1, until)))
        })
        .await;

        let commodities = &acquired.systems[0].markets[0].commodities;
        let producer = commodities
            .iter()
            .find(|row| row.side == Side::Producer)
            .unwrap();
        assert_eq!(producer.supply_price(), Some(100));
        assert_eq!(producer.demand_price(), None);
        let consumer = commodities
            .iter()
            .find(|row| row.side == Side::Consumer)
            .unwrap();
        assert_eq!(
            consumer.supply_price(),
            None,
            "stray buyPrice is not stock evidence"
        );
        assert_eq!(consumer.demand_price(), Some(500));
        assert_eq!(
            consumer.illegal_jurisdiction_code, 46,
            "the Qty suffix is a code"
        );
    }

    #[tokio::test]
    async fn transport_and_whole_document_failures_are_reported_by_batch() {
        let fs = RecordingFs::default();
        let cache = Cache::with_max_age_ms(PathBuf::from("/cache"), 86_400_000.0, true, false);
        let turn = RefCell::new(0);
        let acquired = acquire(&[1, 2, 3], &cache, &fs, NOW_MS, rules(1), |batch| {
            let current = *turn.borrow();
            *turn.borrow_mut() += 1;
            let result = match current {
                0 => Err("offline".to_owned()),
                1 => Ok("not-json".to_owned()),
                _ => Ok(r#"{"wrong":{}}"#.to_owned()),
            };
            assert_eq!(batch.len(), 1);
            std::future::ready(result)
        })
        .await;

        assert_eq!(acquired.failed_batches.len(), 3);
        assert_eq!(acquired.missing, vec![1, 2, 3]);
        assert!(acquired.systems.is_empty());
    }
}
