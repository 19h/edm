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
    /// Its market id when the Ardent station row supplied one.
    pub market_id: Option<f64>,
}

/// Why a request did not produce a document.
///
/// The status is kept because "no such system" and "the request failed" are
/// different facts that a caller has to report differently, and `None` — a
/// transport failure with no response at all — is a third.
#[derive(Clone, Debug)]
pub struct Refusal {
    pub status: Option<u16>,
    pub message: String,
}

pub struct ArdentClient<'a, H> {
    http: &'a H,
    base: &'a str,
}

impl<H> std::fmt::Debug for ArdentClient<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArdentClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl<'a, H: HttpTransport> ArdentClient<'a, H> {
    pub const fn new(http: &'a H, base: &'a str) -> Self {
        Self { http, base }
    }

    /// `fetchArdentJson` (ts:2478).
    async fn fetch_json(&self, url: &str) -> Result<JsValue, String> {
        self.fetch_json_status(url)
            .await
            .map_err(|refusal| refusal.message)
    }

    /// The same fetch, with the status kept.
    ///
    /// A caller that has to tell "no such system" from "the request failed"
    /// needs the status, and reading it back out of the message would be
    /// sniffing a string this module formats — a coupling that fails silently
    /// the day the wording changes.
    async fn fetch_json_status(&self, url: &str) -> Result<JsValue, Refusal> {
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
            .map_err(|e| Refusal {
                status: None,
                message: e.to_string(),
            })?;

        if !(200..300).contains(&response.status) {
            return Err(Refusal {
                status: Some(response.status),
                message: format!(
                    "Ardent replied HTTP {} {} for {url}",
                    response.status, response.status_text
                ),
            });
        }
        // `response.json()` throws on a malformed body, and that throw is not
        // caught on two of the three call sites below.
        JsValue::parse(&response.body).map_err(|e| Refusal {
            status: Some(response.status),
            message: e.to_string(),
        })
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
                    market_id: None,
                });
            }
            if kind == Lookup::System {
                return Err(ardent::unknown_system(name));
            }
        }

        // Station search matches on prefix, so an exact hit wins over a unique
        // prefix hit.
        let payload = self
            .fetch_json(&ardent::station_search_url(self.base, name))
            .await?;
        let matches: Vec<StationMatch> = ardent::parse_station_matches(&payload);
        let chosen = ardent::choose_station(&matches, name)?;

        let payload = self
            .fetch_json(&ardent::system_url(self.base, &chosen.system_name))
            .await?;
        let system = ardent::parse_system(&payload).ok_or_else(|| {
            ardent::unknown_station_system(&chosen.station_name, &chosen.system_name)
        })?;

        Ok(ResolvedSystem {
            system,
            via: format!("station \"{}\"", chosen.station_name),
            station: Some(chosen.station_name.clone()),
            market_id: chosen.market_id,
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

    /// Price-ranked sellers or buyers for one named commodity near a system.
    ///
    /// These are *candidates*, not authoritative listings: Ardent's index is
    /// what makes a quick lookup small, while the caller still polls every
    /// chosen market through Frontier before it ranks or relays anything.
    pub async fn commodity_nearby(
        &self,
        system_name: &str,
        commodity: &str,
        direction: ardent::CommodityDirection,
        max_distance_ly: f64,
        include_carriers: bool,
        min_volume: f64,
    ) -> Result<Vec<ardent::CommodityPrice>, String> {
        let url = ardent::commodity_nearby_url(
            self.base,
            system_name,
            commodity,
            direction,
            max_distance_ly,
            include_carriers,
            min_volume,
        );
        Ok(ardent::parse_commodity_prices(
            &self.fetch_json(&url).await?,
            direction,
        ))
    }

    /// Every commodity id Ardent indexes, through a local copy when there is a
    /// fresh one.
    ///
    /// Cached on the same terms as the galaxy's shape: the catalogue changes
    /// when Frontier adds a commodity, which is a matter of game updates, not of
    /// market activity.
    /// The second value is whether this cost a request, so a caller that reports
    /// its Ardent spend does not have to guess.
    pub async fn commodity_catalogue_cached<F: Fs>(
        &self,
        atlas: &Atlas,
        fs: &F,
        now_ms: f64,
    ) -> Result<(Vec<String>, bool), String> {
        let url = ardent::commodities_url(self.base);
        if let Some(body) = atlas.get(fs, &url, now_ms, atlas::NEARBY_LIFETIME_MINUTES) {
            return Ok((ardent::parse_commodity_ids(&body), false));
        }
        let body = self.fetch_json(&url).await?;
        atlas.put(fs, &url, &body, now_ms);
        Ok((ardent::parse_commodity_ids(&body), true))
    }

    /// Both price sides for the reference system itself.
    ///
    /// Ardent's nearby price endpoint deliberately excludes its centre. This
    /// direct commodity route restores those zero-Ly candidates without making
    /// a regional market-list survey; the caller combines and locally ranks
    /// these rows with the nearby prefixes before any live poll.
    pub async fn commodity_in_system(
        &self,
        system_name: &str,
        commodity: &str,
    ) -> Result<(Vec<ardent::CommodityPrice>, Vec<ardent::CommodityPrice>), String> {
        let url = ardent::system_commodity_url(self.base, system_name, commodity);
        let value = self.fetch_json(&url).await?;
        Ok((
            ardent::parse_commodity_prices(&value, ardent::CommodityDirection::Exports),
            ardent::parse_commodity_prices(&value, ardent::CommodityDirection::Imports),
        ))
    }

    /// The same page, read through a local copy when there is a fresh one.
    ///
    /// A separate method rather than a field on the client, because the four
    /// ported commands must keep making exactly the requests they make today
    /// \[R50\] — a cache under them would change the wire log the parity
    /// harness diffs. Only regional extension commands call this.
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

    /// One system's station list, read through the local copy when fresh, with
    /// the status kept for a caller that must tell an unknown system from a
    /// failed request.
    pub async fn system_markets_cached_status<F: Fs>(
        &self,
        atlas: &Atlas,
        fs: &F,
        now_ms: f64,
        system: &ReferenceSystem,
    ) -> Result<Vec<ardent::ArdentStation>, Refusal> {
        let url = ardent::system_markets_url(self.base, &system.name);
        let body = if let Some(body) = atlas.get(fs, &url, now_ms, atlas::MARKETS_LIFETIME_MINUTES)
        {
            body
        } else {
            let fetched = self.fetch_json_status(&url).await?;
            atlas.put(fs, &url, &fetched, now_ms);
            fetched
        };
        let mut stations = ardent::parse_system_markets(&body);
        ardent::place(&mut stations, system.address, system.coordinates);
        Ok(stations)
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
        ardent::place(&mut stations, system.address, system.coordinates);
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
        self.system_markets_status(system)
            .await
            .map_err(|refusal| refusal.message)
    }

    /// The same list, with the status kept for a caller that must tell an
    /// unknown system from a failed request.
    pub async fn system_markets_status(
        &self,
        system: &ReferenceSystem,
    ) -> Result<Vec<ardent::ArdentStation>, Refusal> {
        let url = ardent::system_markets_url(self.base, &system.name);
        let mut stations = ardent::parse_system_markets(&self.fetch_json_status(&url).await?);
        ardent::place(&mut stations, system.address, system.coordinates);
        Ok(stations)
    }

    /// The only route from a bare market id back to the names EDDN requires
    /// (ts:2974).
    ///
    /// Swallows everything — a transport failure, a non-2xx, a malformed body,
    /// a record missing either name — because the caller reports the same
    /// "Ardent does not know market X" for all of them. R81.
    pub async fn station_by_market_id(&self, market_id: f64) -> Option<EddnStation> {
        let payload = self
            .fetch_json(&ardent::market_url(self.base, market_id))
            .await
            .ok()?;
        let (system_name, station_name, station_type) = ardent::parse_market_station(&payload)?;
        Some(EddnStation {
            system_name,
            station_name,
            station_type,
            economies: None,
        })
    }
}
