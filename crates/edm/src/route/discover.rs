//! Enumerating the systems in a radius, and bounding what that enumeration
//! promises.
//!
//! Ardent's `/nearby` returns at most 1,000 rows and clamps `maxDistance` to
//! 500 Ly, and it says neither. Around Sol the cap binds at about 46 Ly, so a
//! naive "ask for 50 Ly and trust the answer" silently enumerates a smaller
//! ball and reports a region it never looked at.
//!
//! **What makes this recoverable rather than a guess:** the rows are sorted by
//! ascending distance. A full page is therefore not "some systems are missing"
//! but "every system out to the last row is present, and beyond it I cannot
//! say" — truncation is a statement about the shell past `d_max`, never a hole
//! in the middle. So the ball out to `d_max` is *known complete*, and the
//! frontier can be closed by re-anchoring the same query on systems that fell
//! outside it and unioning the answers.
//!
//! **What it still does not promise.** An anchor can only sit on a system
//! Ardent has already told us about, so the coverage argument is complete only
//! to the density of Ardent's own table, and a system it has never seen is
//! invisible to any number of queries. [`Enumeration::complete_to_ly`] is the
//! radius the claim actually extends to, and [`Enumeration::truncated`] says
//! when the query budget, rather than the geometry, is what ended it. Both are
//! meant to be printed.
//!
//! Nothing here is paced: `/nearby` is CDN-fronted, free and unmetered, and it
//! is the Companion API — one request per market — that the pacer and the
//! spend gate exist for.

use std::collections::HashSet;

use edm_core::ardent::{
    ARDENT_MAX_DISTANCE_LY, NEARBY_ROUNDING_SLACK_LY, NEARBY_ROW_CAP, NearbyPage, NearbySystem,
    ReferenceSystem, separation_ly,
};
use edm_core::domain::id64::Coordinates;
use edm_core::js;

use crate::ardent::ArdentClient;
use crate::net::HttpTransport;

/// Called before each re-anchored query.
///
/// Carries the system count as well as the completeness radius, because the
/// radius is the number a user cares about and the count is the number that
/// *moves*. `frontier` anchors on the **farthest** uncovered system, so the
/// nearest uncovered one — and hence the completeness claim — can sit still for
/// dozens of anchors while the shell fills in behind it. A progress line
/// showing only the radius reads as a stall.
pub type AnchorReport<'a> = &'a dyn Fn(&AnchorProgress<'_>);

/// What an anchor is about to do, and what is known so far.
#[derive(Clone, Copy, Debug)]
pub struct AnchorProgress<'a> {
    pub anchor: u32,
    pub budget: u32,
    pub system: &'a str,
    pub systems_known: usize,
    pub complete_to_ly: f64,
}

/// How many `/nearby` requests one enumeration may spend.
///
/// The expansion terminates on its own — every anchor is a distinct system and
/// covers a ball that contains it, so no system is ever anchored twice — but
/// "terminates" is not the same as "terminates soon", and a dense core near Sol
/// can want hundreds of anchors. This bounds it regardless, at the cost of an
/// answer that says it is incomplete.
pub const DEFAULT_ANCHOR_BUDGET: u32 = 200;

/// Every system found in the radius, and the strength of that claim.
#[derive(Clone, Debug)]
pub struct Enumeration {
    /// Deduplicated by system address, the enumeration centre first and the
    /// rest in the order they were discovered — which, for the opening query,
    /// is nearest first.
    ///
    /// Each row's `distance` has been re-based onto the centre as an exact
    /// separation, so it is comparable across rows that arrived from different
    /// anchors; Ardent's own rounded, anchor-relative column does not survive
    /// the union.
    pub systems: Vec<NearbySystem>,
    /// How many `/nearby` requests this cost.
    pub ardent_requests: u32,
    /// The radius within which coverage is *complete*: every system Ardent
    /// knows this close to the centre is in [`systems`](Self::systems).
    ///
    /// Equal to the requested radius (clamped to [`ARDENT_MAX_DISTANCE_LY`])
    /// when the frontier closed. Otherwise it is the distance to the nearest
    /// system known to sit outside every enumerated ball — the first place the
    /// enumeration stops being able to speak for itself.
    pub complete_to_ly: f64,
    /// Whether the budget, rather than the geometry, ended the expansion.
    pub truncated: bool,
    /// How many re-anchored queries the row cap forced.
    pub anchors_used: u32,
}

/// A ball whose interior Ardent has enumerated for us in full.
#[derive(Clone, Copy, Debug)]
struct Covered {
    centre: Coordinates,
    radius_ly: f64,
}

/// Enumerates every system within `radius_ly` of `centre`, expanding around the
/// row cap until the frontier closes or `budget` requests are spent.
///
/// The opening query is always sent, even at `budget` zero: an enumeration with
/// no rows at all is not a cheaper answer, it is no answer.
///
/// Errors are not swallowed. An Ardent outage that read as an empty region
/// would be indistinguishable from a genuinely empty one, and the whole command
/// downstream is a claim about completeness.
#[expect(clippy::too_many_arguments, reason = "a client, its cache, and the query it answers")]
pub async fn enumerate<H: HttpTransport, F: crate::ports::Fs>(
    ardent: &ArdentClient<'_, H>,
    atlas: &crate::route::atlas::Atlas,
    fs: &F,
    now_ms: f64,
    centre: &ReferenceSystem,
    radius_ly: f64,
    budget: u32,
    report: Option<AnchorReport<'_>>,
) -> Result<Enumeration, String> {
    // Asking for more than the server honours does not fail, it quietly
    // narrows the answer — so completeness may never be claimed past the clamp.
    let reach = js::js_min(radius_ly, ARDENT_MAX_DISTANCE_LY);

    // Ardent excludes the queried system from its own `/nearby` answer
    // (measured: Sol's begins at 1 Ly), and the reference system is exactly the
    // one a route is most likely to start from.
    let mut systems = vec![NearbySystem {
        name: centre.name.clone(),
        address: centre.address,
        coordinates: centre.coordinates,
        distance: 0.0,
    }];
    let mut seen: HashSet<u64> = HashSet::from([address_key(centre.address)]);

    let page = ardent.nearby_cached(atlas, fs, now_ms, &centre.name, radius_ly).await?;
    let mut requests = 1u32;
    let mut anchors = 0u32;
    let mut balls =
        vec![Covered { centre: centre.coordinates, radius_ly: covered_to(&page, reach) }];
    absorb(&mut systems, &mut seen, centre.coordinates, radius_ly, page);

    let (complete_to_ly, truncated) = loop {
        let Some(frontier) = frontier(&systems, radius_ly, &balls) else {
            break (reach, false);
        };
        if requests >= budget {
            break (frontier.nearest_ly, true);
        }

        // The farthest uncovered system, so each anchor reaches as much of the
        // unclaimed shell as one query can.
        let anchor = systems[frontier.farthest].clone();
        // Announced before it is issued, not after. This is the phase where a
        // user most needs to know whether the budget is about to truncate the
        // claim they are going to act on — and at radius 100 it is 41 serial
        // round trips for a 144 KB page each, which is a long time to say
        // nothing.
        if let Some(report) = report {
            report(&AnchorProgress {
                anchor: anchors + 1,
                budget,
                system: &anchor.name,
                systems_known: systems.len(),
                complete_to_ly: frontier.nearest_ly,
            });
        }
        let page = ardent.nearby_cached(atlas, fs, now_ms, &anchor.name, radius_ly).await?;
        requests += 1;
        anchors += 1;
        balls.push(Covered { centre: anchor.coordinates, radius_ly: covered_to(&page, reach) });
        absorb(&mut systems, &mut seen, centre.coordinates, radius_ly, page);
    };

    // An anchored query is a ball around the anchor, so it returns systems the
    // caller never asked about. Distances are the recomputed ones; Ardent's
    // rounded column would keep rows up to half a light year outside.
    systems.retain(|system| system.distance <= radius_ly);

    Ok(Enumeration {
        systems,
        ardent_requests: requests,
        complete_to_ly,
        truncated,
        anchors_used: anchors,
    })
}

/// How far one page's completeness claim reaches.
///
/// A short page exhausted the radius, so it covers everything the server was
/// willing to consider. A full page covers out to its last row's *reported*
/// distance, less the rounding slack — see [`NEARBY_ROUNDING_SLACK_LY`] for why
/// the reported integer is the right quantity here and the recomputed
/// separation is not.
fn covered_to(page: &NearbyPage, reach: f64) -> f64 {
    if page.rows < NEARBY_ROW_CAP {
        return reach;
    }
    let mut furthest = 0.0;
    for system in &page.systems {
        furthest = js::js_max(furthest, system.distance);
    }
    js::js_min(reach, js::js_max(0.0, furthest - NEARBY_ROUNDING_SLACK_LY))
}

/// The systems inside the radius that no enumerated ball speaks for.
struct Frontier {
    /// Index of the farthest such system from the centre — the next anchor.
    farthest: usize,
    /// Distance to the nearest such system: the radius at which the
    /// enumeration stops being complete.
    nearest_ly: f64,
}

fn frontier(systems: &[NearbySystem], radius_ly: f64, balls: &[Covered]) -> Option<Frontier> {
    let mut farthest: Option<(usize, f64)> = None;
    let mut nearest_ly = f64::INFINITY;

    for (index, system) in systems.iter().enumerate() {
        // Re-based onto the centre by `absorb`, so it is a real separation.
        let distance = system.distance;
        if distance > radius_ly {
            continue;
        }
        let covered = balls
            .iter()
            .any(|ball| separation_ly(&ball.centre, &system.coordinates) <= ball.radius_ly);
        if covered {
            continue;
        }
        // Strictly greater, so a tie keeps the first — the enumeration must not
        // depend on which of two equidistant systems Ardent listed second.
        if farthest.is_none_or(|(_, best)| distance > best) {
            farthest = Some((index, distance));
        }
        if distance < nearest_ly {
            nearest_ly = distance;
        }
    }

    farthest.map(|(index, _)| Frontier { farthest: index, nearest_ly })
}

/// Unions a page into the accumulated set, first row of an address winning.
fn absorb(
    systems: &mut Vec<NearbySystem>,
    seen: &mut HashSet<u64>,
    centre: Coordinates,
    radius_ly: f64,
    page: NearbyPage,
) {
    for mut system in page.systems {
        if !seen.insert(address_key(system.address)) {
            continue;
        }
        system.distance = separation_ly(&centre, &system.coordinates);
        // **Outside the ball is not in the answer.** An anchored query is a
        // ball around the *anchor*, so at radius 100 it returns systems up to
        // 100 Ly beyond it — twice the requested radius from the centre. Those
        // rows were kept, counted in the plan as "N in radius", and then had a
        // market list read for each of them: at radius 100 around Sol that is
        // thousands of free-but-not-instant Ardent requests spent on systems no
        // route could ever use, and a systems count that was simply untrue.
        //
        // `seen` still remembers them, so a later anchor does not re-absorb the
        // same row, and `frontier` already refused to anchor on one.
        if system.distance <= radius_ly {
            systems.push(system);
        }
    }
}

/// A system address as a hash key.
///
/// Addresses arrive as `f64` because that is what the whole port reads JSON
/// numbers into, and `f64` is neither `Eq` nor `Hash`. The bit pattern is a
/// sound key for the integers Ardent sends: the only two distinct patterns that
/// compare equal are `+0` and `-0`, and JSON has no `-0` address.
fn address_key(address: f64) -> u64 {
    address.to_bits()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use edm_core::ardent::{NEARBY_ROW_CAP, ReferenceSystem, separation_ly};
    use edm_core::domain::id64::Coordinates;
    use edm_core::js;

    use super::{DEFAULT_ANCHOR_BUDGET, Enumeration, enumerate};
    use crate::ardent::ArdentClient;
    use crate::net::{HeaderView, HttpRequest, HttpResponse, HttpTransport, TransportError};

    const BASE: &str = "http://ardent.test";
    const ORIGIN: Coordinates = Coordinates { x: 0.0, y: 0.0, z: 0.0 };
    const SOL_ADDRESS: f64 = 10_477_373_803.0;

    /// A transport that answers `/nearby` from a script keyed on the system
    /// name in the path, and refuses anything unscripted so that a test cannot
    /// pass by accidentally enumerating nothing.
    #[derive(Debug, Default)]
    struct FakeArdent {
        pages: HashMap<String, String>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeArdent {
        fn page(mut self, system: &str, body: String) -> Self {
            self.pages.insert(system.to_owned(), body);
            self
        }
    }

    impl HttpTransport for FakeArdent {
        async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
            self.calls.borrow_mut().push(request.url.to_owned());
            let path = request.url.split_once('?').map_or(request.url, |(head, _)| head);
            let name = path
                .split_once("/system/name/")
                .and_then(|(_, rest)| rest.split('/').next())
                .unwrap_or_default();
            let body = self
                .pages
                .get(name)
                .ok_or_else(|| TransportError::Other(format!("no scripted page for {name}")))?;
            Ok(HttpResponse {
                status: 200,
                status_text: "OK".to_owned(),
                headers: HeaderView::default(),
                body: body.clone(),
            })
        }
    }

    fn centre() -> ReferenceSystem {
        ReferenceSystem { name: "Sol".to_owned(), address: SOL_ADDRESS, coordinates: ORIGIN }
    }

    /// A point `x` light years along one axis — enough geometry for a cap that
    /// is about ordering rather than about shape.
    const fn at(x: f64) -> Coordinates {
        Coordinates { x, y: 0.0, z: 0.0 }
    }

    /// One row, carrying the `distance` column Ardent would compute for it:
    /// rounded to whole light years and radial from the queried system.
    fn row(name: &str, address: f64, at: Coordinates, from: Coordinates) -> String {
        let reported = js::js_round(separation_ly(&from, &at));
        format!(
            r#"{{"systemName":"{name}","systemAddress":{},"systemX":{},"systemY":{},"systemZ":{},"distance":{}}}"#,
            js::js_number(address),
            js::js_number(at.x),
            js::js_number(at.y),
            js::js_number(at.z),
            js::js_number(reported),
        )
    }

    fn page(from: Coordinates, entries: &[(&str, f64, Coordinates)]) -> String {
        let rows: Vec<String> =
            entries.iter().map(|(name, address, at)| row(name, *address, *at, from)).collect();
        format!("[{}]", rows.join(","))
    }

    /// `count` systems strung out along +x from `from`, one every `step` light
    /// years — already in the ascending order Ardent sorts by.
    fn line(from: Coordinates, count: usize, step: f64, prefix: &str, first: f64) -> String {
        let entries: Vec<(String, f64, Coordinates)> = (1..=count)
            .map(|k| (format!("{prefix}{k:04}"), first + k as f64, at(from.x + step * k as f64)))
            .collect();
        let borrowed: Vec<(&str, f64, Coordinates)> =
            entries.iter().map(|(name, address, at)| (name.as_str(), *address, *at)).collect();
        page(from, &borrowed)
    }

    fn names(enumeration: &Enumeration) -> Vec<&str> {
        enumeration.systems.iter().map(|system| system.name.as_str()).collect()
    }

    async fn run(http: &FakeArdent, radius_ly: f64, budget: u32) -> Enumeration {
        let client = ArdentClient::new(http, BASE);
        enumerate(
            &client,
            &crate::route::atlas::Atlas::new(std::path::Path::new("/none"), false, false),
            &crate::ports::RecordingFs::default(),
            0.0,
            &centre(),
            radius_ly,
            budget,
            None,
        )
        .await
        .expect("the script answers")
    }

    /// A page under the cap exhausted the radius, so one request settles it.
    #[tokio::test]
    async fn an_answer_under_the_row_cap_is_complete_after_one_request() {
        let http = FakeArdent::default().page(
            "Sol",
            page(
                ORIGIN,
                &[
                    ("Alpha Centauri", 1.0, Coordinates { x: 3.03125, y: -0.09375, z: 3.15625 }),
                    ("Barnard's Star", 2.0, Coordinates { x: -3.03125, y: 1.375, z: 4.9375 }),
                ],
            ),
        );

        let found = run(&http, 20.0, DEFAULT_ANCHOR_BUDGET).await;

        assert_eq!(found.ardent_requests, 1);
        assert_eq!(found.anchors_used, 0);
        assert!(!found.truncated);
        assert_eq!(found.complete_to_ly, 20.0);
        // The centre is absent from its own `/nearby` answer, and is added here.
        assert_eq!(names(&found), ["Sol", "Alpha Centauri", "Barnard's Star"]);
    }

    /// A full page bounds coverage at its last row, and the expansion closes
    /// what is left. The second query's rows are unioned, not appended twice.
    #[tokio::test]
    async fn a_full_page_forces_an_expansion_that_closes_the_frontier() {
        // 1,000 systems every 0.01 Ly, so the last sits at 10 Ly and the page is
        // exactly full: coverage reaches 10 - 0.5 = 9.5 Ly, and the fifty
        // systems beyond that are the frontier.
        let core = line(ORIGIN, NEARBY_ROW_CAP, 0.01, "S", 1000.0);
        let http = FakeArdent::default().page("Sol", core).page(
            "S1000",
            page(
                at(10.0),
                &[
                    // Already known, by address: the union must not grow for it.
                    ("S0999", 1999.0, at(9.99)),
                    ("Rim", 5001.0, at(10.5)),
                    ("Outer Rim", 5002.0, at(15.0)),
                ],
            ),
        );

        let found = run(&http, 20.0, DEFAULT_ANCHOR_BUDGET).await;

        assert_eq!(found.ardent_requests, 2);
        assert_eq!(found.anchors_used, 1);
        assert!(!found.truncated);
        assert_eq!(found.complete_to_ly, 20.0);
        assert_eq!(found.systems.len(), 1 + NEARBY_ROW_CAP + 2);
        assert_eq!(found.systems.iter().filter(|system| system.name == "S0999").count(), 1);

        // The anchor was the farthest uncovered system, not merely an uncovered
        // one.
        assert!(http.calls.borrow()[1].contains("/system/name/S1000/nearby"));
    }

    /// Out of budget, the answer says how far it can still speak for — which is
    /// further than the opening page alone, and short of the whole radius.
    #[tokio::test]
    async fn the_budget_bounds_the_expansion_and_the_claim_shrinks_to_match() {
        let http =
            FakeArdent::default().page("Sol", line(ORIGIN, NEARBY_ROW_CAP, 0.01, "S", 1000.0));

        let found = run(&http, 20.0, 1).await;

        assert_eq!(found.ardent_requests, 1);
        assert_eq!(found.anchors_used, 0);
        assert!(found.truncated);
        // Coverage reaches 9.5 Ly; the first system it cannot speak for sits at
        // 9.51, and that is what is claimed — never the 20 that was asked for.
        let claimed = found.complete_to_ly;
        assert!((claimed - 9.51).abs() < 1e-9, "{}", js::js_number(claimed));
        assert_eq!(found.systems.len(), 1 + NEARBY_ROW_CAP);
    }

    /// An anchor whose own page is full extends the claim without completing
    /// it: the union reaches further than any single query could, and what is
    /// left over is reported rather than rounded up to the radius.
    #[tokio::test]
    async fn coverage_grows_with_each_anchor_and_stops_where_the_budget_does() {
        let http = FakeArdent::default()
            .page("Sol", line(ORIGIN, NEARBY_ROW_CAP, 0.01, "S", 1000.0))
            // Dense enough that a full page reaches only 1 Ly, so this anchor
            // covers [9.5, 10.5] and the systems past it stay unclaimed.
            .page("S1000", line(at(10.0), NEARBY_ROW_CAP, 0.001, "R", 5000.0));

        let found = run(&http, 20.0, 2).await;

        assert_eq!(found.ardent_requests, 2);
        assert_eq!(found.anchors_used, 1);
        assert!(found.truncated);
        // 9.5 was the opening page's reach; the anchor pushed the claim out to
        // the first system it in turn could not speak for, at 10.501.
        let claimed = found.complete_to_ly;
        assert!((claimed - 10.501).abs() < 1e-9, "{}", js::js_number(claimed));
        assert_eq!(found.systems.len(), 1 + 2 * NEARBY_ROW_CAP);
    }

    /// Two rows for one address are one system, and the first name wins.
    #[tokio::test]
    async fn systems_are_deduplicated_by_address() {
        let http = FakeArdent::default().page(
            "Sol",
            page(
                ORIGIN,
                &[
                    ("Alpha", 11.0, at(1.0)),
                    ("Alpha Again", 11.0, at(1.0)),
                    // Ardent omits the queried system, but a re-anchored query
                    // returns it, so the centre has to dedupe as well.
                    ("Sol", SOL_ADDRESS, ORIGIN),
                ],
            ),
        );

        let found = run(&http, 20.0, DEFAULT_ANCHOR_BUDGET).await;

        assert_eq!(names(&found), ["Sol", "Alpha"]);
    }

    /// `maxDistance` is clamped at 500 Ly without a word, so a wider request
    /// cannot buy a wider claim — whatever the request line said.
    #[tokio::test]
    async fn the_silent_radius_clamp_caps_what_completeness_can_mean() {
        let http = FakeArdent::default().page("Sol", page(ORIGIN, &[("Far", 7.0, at(480.0))]));

        let found = run(&http, 600.0, DEFAULT_ANCHOR_BUDGET).await;

        assert_eq!(found.complete_to_ly, 500.0);
        assert!(!found.truncated);
        assert!(http.calls.borrow()[0].ends_with("maxDistance=600"));
    }

    /// The radius filter runs on recomputed separations. Ardent's column is
    /// rounded, so trusting it would keep rows half a light year outside and
    /// drop rows inside.
    #[tokio::test]
    async fn rows_outside_the_radius_are_dropped_after_recomputation() {
        let http = FakeArdent::default().page(
            "Sol",
            page(ORIGIN, &[("Inside", 31.0, at(19.6)), ("Outside", 32.0, at(20.4))]),
        );

        let found = run(&http, 20.0, DEFAULT_ANCHOR_BUDGET).await;

        // Ardent reports 20 for both rows; only one of them is within 20 Ly.
        assert_eq!(names(&found), ["Sol", "Inside"]);
    }
}
