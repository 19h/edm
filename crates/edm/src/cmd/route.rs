//! `edm route` — sweep a region for live prices, then rank what is in it.
//!
//! The sequencing here is the safeguard, and it is the reason this file reads
//! as a list of steps rather than a pipeline:
//!
//! 1. Resolve the reference and enumerate the region **through Ardent**, which
//!    is free, unmetered and CDN-fronted.
//! 2. Filter to markets a large ship can actually use, before anything is spent.
//! 3. **Print the plan and price it.** Above the ceiling, stop here.
//! 4. Only then poll the Companion API, paced, one request per market.
//!
//! Steps 1–3 cannot send a Frontier request at all, which is what makes
//! `expect-frontier-requests = 0` a provable assertion in the harness rather
//! than an assumption. A run that refuses is a run whose wire log is empty.

use edm_core::ardent::{self, Lookup, ReferenceSystem};
use edm_core::cli::config::RouteConfig;
use edm_core::render::views::{self, RouteCoverage};
use edm_core::select;
use edm_core::spend::{Counts, SizePrior};

use crate::ardent::ArdentClient;
use crate::cmd::{App, CmdResult};
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};
use crate::route::discover::{self, DEFAULT_ANCHOR_BUDGET};
use crate::route::plan::{self, Survey};

/// Run the command.
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    config: &RouteConfig,
) -> CmdResult {
    let out = app.out;
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);

    // Nothing below this point may run on a name that was never resolved: an
    // enumeration centred on the wrong system is a complete, confident answer
    // about the wrong region.
    let centre = resolve(&ardent, &config.reference).await?;

    let budget = if config.ardent_queries == 0 { DEFAULT_ANCHOR_BUDGET } else { config.ardent_queries };
    let enumeration = discover::enumerate(&ardent, &centre, config.radius_ly, budget)
        .await
        .map_err(|error| format!("enumerating systems around {}: {error}", centre.name))?;

    // One free `/markets` per system, then the filter. Both happen before the
    // plan is priced, so the plan's market count is measured rather than
    // extrapolated — `--fast-estimate` is the flag that trades this away.
    let (stations, systems_with_markets) = if config.fast_estimate {
        (Vec::new(), enumeration.systems.len())
    } else {
        gather(&ardent, &enumeration).await
    };

    let selection = select::select(stations, config, &centre.coordinates);
    let systems_to_read =
        if config.fast_estimate { systems_with_markets } else { systems_holding(&selection) };

    let survey = Survey {
        complete_to_ly: enumeration.complete_to_ly,
        ardent_requests: enumeration.ardent_requests,
        counts: Counts {
            systems: enumeration.systems.len(),
            systems_to_read,
            stations_known: selection.considered,
            markets_to_poll: selection.keep.len(),
            cached_fresh: 0,
        },
        exclusions: selection.exclusions.clone(),
    };

    let decision = plan::gate(out, config, &survey, SizePrior::default());
    if !decision.proceeds() {
        return Ok(());
    }

    // The sweep itself is not yet wired; a run that reaches here has been
    // priced and permitted and has sent nothing.
    let coverage = RouteCoverage {
        systems_total: survey.counts.systems_to_read,
        markets_found: selection.keep.len(),
        truncated_to_ly: enumeration.truncated.then_some(enumeration.complete_to_ly),
        ..RouteCoverage::default()
    };
    out.emit(&views::route_coverage(&coverage));
    Ok(())
}

/// The reference system, as a point to enumerate around.
///
/// `Lookup::Auto` so `edm route "Jaques Station"` works — but the radius is
/// measured from the *system*, which is the only thing a light year is a
/// distance between, and the station name only ever selects it.
async fn resolve<H: HttpTransport>(
    ardent: &ArdentClient<'_, H>,
    reference: &str,
) -> Result<ReferenceSystem, String> {
    Ok(ardent.resolve_location(reference, Lookup::Auto).await?.system)
}

/// One `/markets` per enumerated system.
///
/// Free and unmetered, so this is not paced and failures are not retried: a
/// system whose market list does not answer contributes nothing, and the plan
/// reports a smaller region rather than a wrong one. Returns the stations and
/// how many systems answered at all.
async fn gather<H: HttpTransport>(
    ardent: &ArdentClient<'_, H>,
    enumeration: &discover::Enumeration,
) -> (Vec<ardent::ArdentStation>, usize) {
    let mut stations = Vec::new();
    let mut answered = 0;
    for system in &enumeration.systems {
        // `system_markets` places the rows at these coordinates itself, which
        // is the only reason a `/markets` row has any position at all.
        let reference = ReferenceSystem {
            name: system.name.clone(),
            address: system.address,
            coordinates: system.coordinates,
        };
        let Ok(mut found) = ardent.system_markets(&reference).await else { continue };
        answered += 1;
        stations.append(&mut found);
    }
    (stations, answered)
}

/// How many systems still hold a market worth reading.
///
/// This is the number the plan prices, not the number of systems in radius:
/// near Sol the filter empties most of them, and a starsystem read is the
/// larger of the two request kinds by a factor of twenty-five.
fn systems_holding(selection: &select::Selection) -> usize {
    let mut names: Vec<&str> = selection.keep.iter().map(|s| s.system_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::ardent::ArdentStation;
    use edm_core::domain::id64::Coordinates;

    fn station(system: &str) -> ArdentStation {
        ArdentStation {
            market_id: 1.0,
            station_name: "S".to_owned(),
            system_name: system.to_owned(),
            station_type: Some("Coriolis".to_owned()),
            max_landing_pad_size: None,
            distance_to_arrival: None,
            coordinates: Coordinates { x: 0.0, y: 0.0, z: 0.0 },
        }
    }

    /// A starsystem read is ~500 KB against a market's ~20 KB, so pricing one
    /// per system in radius rather than per system that still holds a market
    /// would overstate the transfer by more than the market reads cost.
    #[test]
    fn only_systems_that_still_hold_a_market_are_priced() {
        let selection = select::Selection {
            keep: vec![station("Sol"), station("Sol"), station("Alpha Centauri")],
            exclusions: Vec::new(),
            considered: 40,
        };
        assert_eq!(systems_holding(&selection), 2);
    }

    #[test]
    fn an_empty_region_prices_no_system_reads() {
        assert_eq!(systems_holding(&select::Selection::default()), 0);
    }
}
