//! Free Ardent reads that feed completion \[C53\].
//!
//! None of these is metered, so none goes through the pacer, and they run
//! beside a network job rather than after it. The atlas caches the answers
//! for a week, so a session's second start costs nothing.

use crate::ardent::ArdentClient;
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};
use crate::route::atlas::Atlas;

use super::{Event, Session};

/// How far around the ship completion looks for system names.
///
/// A hundred light years is a few hundred systems in the bubble: every name a
/// commander is likely to type next, and one cached page.
const NEARBY_LY: f64 = 100.0;

/// The commodity catalogue, and the systems around `system` when known.
pub(crate) async fn warmup<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    system: Option<&str>,
) -> Result<(), String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let ardent = ArdentClient::new(session.http, &session.overrides.ardent_base);
    let atlas = Atlas::new(&session.cache_root, true, false);
    let now_ms = session.ports.clock.now_ms();
    let (catalogue, _) = ardent
        .commodity_catalogue_cached(&atlas, &session.ports.fs, now_ms)
        .await
        .map_err(|error| format!("reading Ardent's commodity catalogue: {error}"))?;
    session.send(Event::Catalogue(catalogue)).await;
    if let Some(system) = system {
        let page = ardent
            .nearby_cached(&atlas, &session.ports.fs, now_ms, system, NEARBY_LY)
            .await
            .map_err(|error| format!("reading the systems around {system}: {error}"))?;
        session.send(Event::Nearby(page.systems)).await;
    }
    Ok(())
}

/// Stations whose names start with `prefix`, with their systems.
pub(crate) async fn station_search<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    prefix: String,
) -> Result<(), String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let ardent = ArdentClient::new(session.http, &session.overrides.ardent_base);
    let matches = ardent.station_matches(&prefix).await?;
    session
        .send(Event::StationMatches {
            query: prefix,
            matches,
        })
        .await;
    Ok(())
}
