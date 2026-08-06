//! Fetching from Ardent Insight.
//!
//! The URL building and parsing are in `edm_core::ardent`, ported from the
//! TypeScript module the original imports at runtime (C1). This is only the
//! part that needs a socket — plus one asymmetry in the original's error
//! handling that is easy to smooth over by accident and must not be.

use edm_core::ardent::{self, Lookup, ReferenceSystem, StationMatch};
use edm_core::domain::eddn::EddnStation;
use edm_core::js::json::JsValue;

use crate::net::{Body, HttpRequest, HttpResponse, HttpTransport, Profile};
use crate::ports::Fs;
use crate::route::atlas::{self, Atlas};

/// A system resolved from a name, and how it was found.
#[derive(Clone, Debug)]
pub struct ResolvedSystem {
    pub system: ReferenceSystem,
    /// `"system name"`, or `station "Jaques Station"` — printed as
    /// "resolved via".
    pub via: String,
    /// The station that led here, when a station search resolved it.
    pub station: Option<String>,
}

pub struct ArdentClient<'a, H> {
    http: &'a H,
    base: &'a str,
}

impl<H> std::fmt::Debug for ArdentClient<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArdentClient").field("base", &self.base).finish_non_exhaustive()
    }
}

impl<'a, H: HttpTransport> ArdentClient<'a, H> {
    pub const fn new(http: &'a H, base: &'a str) -> Self {
        Self { http, base }
    }

    /// `fetchArdentJson` (ts:2478).
    async fn fetch_json(&self, url: &str) -> Result<JsValue, String> {
        let headers = [("accept", "application/json".to_owned())];
        let response: HttpResponse = self
            .http
            .send(HttpRequest {
                profile: Profile::Aux,
                method: "GET",
                url,
                headers: &headers,
                body: Body::None,
            })
            .await
            .map_err(|e| e.to_string())?;

        if !(200..300).contains(&response.status) {
            return Err(format!(
                "Ardent replied HTTP {} {} for {url}",
                response.status, response.status_text
            ));
        }
        // `response.json()` throws on a malformed body, and that throw is not
        // caught on two of the three call sites below.
        JsValue::parse(&response.body).map_err(|e| e.to_string())
    }

    /// `resolveLocation` (ts:2485) — a system name, or a station name via its
    /// system.
    ///
    /// The error handling is deliberately asymmetric and R81 says so: only the
    /// **first** lookup is `.catch`-swallowed, so an Ardent outage during the
    /// direct system probe falls through to a station search, while the same
    /// outage during the station search or the follow-up system lookup is
    /// fatal. Wrapping all three in the same handler would be tidier and wrong.
    pub async fn resolve_location(
        &self,
        name: &str,
        kind: Lookup,
    ) -> Result<ResolvedSystem, String> {
        if kind != Lookup::Station {
            let direct = match self.fetch_json(&ardent::system_url(self.base, name)).await {
                Ok(value) => ardent::parse_system(&value),
                // Swallowed — this is the one that is allowed to fail quietly.
                Err(_) => None,
            };
            if let Some(system) = direct {
                return Ok(ResolvedSystem {
                    system,
                    via: "system name".to_owned(),
                    station: None,
                });
            }
            if kind == Lookup::System {
                return Err(ardent::unknown_system(name));
            }
        }

        // Station search matches on prefix, so an exact hit wins over a unique
        // prefix hit.
        let payload = self.fetch_json(&ardent::station_search_url(self.base, name)).await?;
        let matches: Vec<StationMatch> = ardent::parse_station_matches(&payload);
        let chosen = ardent::choose_station(&matches, name)?;

        let payload = self.fetch_json(&ardent::system_url(self.base, &chosen.system_name)).await?;
        let system = ardent::parse_system(&payload).ok_or_else(|| {
            ardent::unknown_station_system(&chosen.station_name, &chosen.system_name)
        })?;

        Ok(ResolvedSystem {
            system,
            via: format!("station \"{}\"", chosen.station_name),
            station: Some(chosen.station_name.clone()),
        })
    }

    /// `/system/name/{s}/nearby?maxDistance=R` — the systems Ardent knows
    /// inside a radius, nearest first \[C25\].
    ///
    /// Unlike the lookups above, a failure here is fatal to its caller and is
    /// not swallowed: an outage that read as an empty answer would be
    /// indistinguishable from a genuinely sparse region, and `edm route` would
    /// go on to report complete coverage of nothing.
    pub async fn nearby(
        &self,
        system_name: &str,
        max_distance: f64,
    ) -> Result<ardent::NearbyPage, String> {
        let url = ardent::nearby_url(self.base, system_name, max_distance);
        Ok(ardent::parse_nearby_page(&self.fetch_json(&url).await?))
    }

    /// The same page, read through a local copy when there is a fresh one.
    ///
    /// A separate method rather than a field on the client, because the four
    /// ported commands must keep making exactly the requests they make today
    /// \[R50\] — a cache under them would change the wire log the parity
    /// harness diffs. Only `route` calls this.
    pub async fn nearby_cached<F: Fs>(
        &self,
        atlas: &Atlas,
        fs: &F,
        now_ms: f64,
        system_name: &str,
        max_distance: f64,
    ) -> Result<ardent::NearbyPage, String> {
        let url = ardent::nearby_url(self.base, system_name, max_distance);
        if let Some(body) = atlas.get(fs, &url, now_ms, atlas::NEARBY_LIFETIME_MINUTES) {
            return Ok(ardent::parse_nearby_page(&body));
        }
        let body = self.fetch_json(&url).await?;
        atlas.put(fs, &url, &body, now_ms);
        Ok(ardent::parse_nearby_page(&body))
    }

    /// One system's station list, read through the local copy when fresh.
    pub async fn system_markets_cached<F: Fs>(
        &self,
        atlas: &Atlas,
        fs: &F,
        now_ms: f64,
        system: &ReferenceSystem,
    ) -> Result<Vec<ardent::ArdentStation>, String> {
        let url = ardent::system_markets_url(self.base, &system.name);
        let body = if let Some(body) = atlas.get(fs, &url, now_ms, atlas::MARKETS_LIFETIME_MINUTES)
        {
            body
        } else {
            let fetched = self.fetch_json(&url).await?;
            atlas.put(fs, &url, &fetched, now_ms);
            fetched
        };
        let mut stations = ardent::parse_system_markets(&body);
        ardent::place(&mut stations, system.coordinates);
        Ok(stations)
    }

    /// `/system/name/{s}/markets` — every station in one system that trades
    /// commodities, with the type, pad and arrival distance the pre-filter runs
    /// on \[C25\].
    ///
    /// The rows carry no coordinates of their own, so they are placed at the
    /// system's before they are returned.
    pub async fn system_markets(
        &self,
        system: &ReferenceSystem,
    ) -> Result<Vec<ardent::ArdentStation>, String> {
        let url = ardent::system_markets_url(self.base, &system.name);
        let mut stations = ardent::parse_system_markets(&self.fetch_json(&url).await?);
        ardent::place(&mut stations, system.coordinates);
        Ok(stations)
    }

    /// The only route from a bare market id back to the names EDDN requires
    /// (ts:2974).
    ///
    /// Swallows everything — a transport failure, a non-2xx, a malformed body,
    /// a record missing either name — because the caller reports the same
    /// "Ardent does not know market X" for all of them. R81.
    pub async fn station_by_market_id(&self, market_id: f64) -> Option<EddnStation> {
        let payload = self.fetch_json(&ardent::market_url(self.base, market_id)).await.ok()?;
        let (system_name, station_name, station_type) = ardent::parse_market_station(&payload)?;
        Some(EddnStation { system_name, station_name, station_type, economies: None })
    }
}
