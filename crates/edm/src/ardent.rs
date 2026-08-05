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
