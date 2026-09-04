//! The spend gate: everything cheap happens, and is *shown*, before anything
//! expensive.
//!
//! This module lands before the optimiser does, deliberately. A ranking engine
//! with no ceiling is a program that can fire forty thousand authenticated
//! requests at Frontier because someone typed `--radius 100` — and the failure
//! mode of finding that out empirically is an account, not a test run. So the
//! gate ships first, and the scenarios that assert **zero requests sent** pass
//! before there is anything worth sending them for.
//!
//! The arithmetic itself is pure and lives in [`edm_core::spend`]; what is here
//! is the ordering, the printing and the exit code — the observable part.

use edm_core::cli::config::RouteConfig;
use edm_core::render::Block;
use edm_core::render::views::{self, PlanView};
use edm_core::spend::{self, Estimate, Refusal, SizePrior, Verdict};

use crate::out::Out;

/// What the enumeration established, handed to the gate.
///
/// Separate from `discover`'s own types so the gate can be tested — and shipped
/// — without a network, which is the entire point of landing it early.
#[derive(Clone, Debug, PartialEq)]
pub struct Survey {
    /// How wide the enumeration is actually complete to. Equal to the requested
    /// radius when the frontier closed.
    pub complete_to_ly: f64,
    /// True for a bounded commodity price prefix rather than a radial system
    /// enumeration. Rendering must never call that prefix "complete".
    pub price_index: bool,
    pub ardent_requests: u32,
    pub counts: spend::Counts,
    pub exclusions: Vec<spend::Exclusion>,
}

/// The refusals that depend on nothing but the command line.
///
/// **Checked before any work at all**, which is not the same as "before
/// anything is sent". A radius past the ceiling is a fact about the argv: it
/// cannot become acceptable once the region is known, so enumerating the region
/// first spends minutes of Ardent queries to reach a conclusion that was
/// available immediately. Reported live at `--radius 200`, where the run sat
/// through nine anchor queries before being told the ceiling is 100.
#[must_use]
pub fn preflight(config: &RouteConfig) -> Option<Refusal> {
    (config.radius_ly > spend::MAX_RADIUS_LY).then_some(Refusal::RadiusTooWide)
}

/// The message a pre-flight refusal prints.
pub fn refuse(out: &Out, config: &RouteConfig, refusal: &Refusal) {
    out.error_paragraph(&spend::refusal_message(
        refusal,
        &Estimate::build(
            spend::Counts::default(),
            Vec::new(),
            config.rate_per_second,
            &SizePrior::default(),
        ),
        config.radius_ly,
        config.max_requests,
    ));
    out.set_exit(crate::out::EXIT_FAILURE);
}

/// The gate's answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Poll the markets.
    Sweep(Estimate),
    /// The plan was printed and that is all that was asked for.
    Stopped(Estimate),
    /// This phase is skipped and the run continues \[C37\].
    ///
    /// Only ever returned for a [`Stage::Intermediate`] gate under
    /// `--dry-run`. A run can now be gated more than once, and treating a
    /// dry-run stop at the *first* gate as the end of the run would show the
    /// carrier-access plan and hide the sweep plan behind it — strictly less
    /// than `--dry-run` printed before, for a flag whose whole purpose is to
    /// print what would happen.
    Skipped(Estimate),
    /// Nothing was sent, and nothing will be.
    Refused(Refusal),
}

/// Whether this gate is the last one in the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Another gate follows; a `--dry-run` should fall through to it.
    Intermediate,
    /// The last gate. `--dry-run` stops here.
    Final,
}

impl Decision {
    /// Whether the caller may proceed to spend requests.
    ///
    /// A method rather than a `matches!` at each call site, so that adding a
    /// fourth variant later cannot silently be read as permission.
    #[must_use]
    pub fn proceeds(&self) -> bool {
        matches!(self, Self::Sweep(_))
    }

    /// Whether the run as a whole should stop here.
    ///
    /// [`Decision::Skipped`] is the one case that is neither permission nor an
    /// ending: this phase sends nothing and the next one still gets its turn.
    #[must_use]
    pub fn ends_the_run(&self) -> bool {
        matches!(self, Self::Stopped(_) | Self::Refused(_))
    }
}

/// Who answers when a sweep needs consent.
///
/// The console cannot ask: it prints the instruction and stops, and the user
/// re-runs with `--yes`. A full-screen UI can, and does, with the same plan in
/// a modal \[C53\]. Everything else about the gate — the pricing, the table,
/// the refusals, the order they appear in — is the same for both.
#[allow(async_fn_in_trait)]
pub trait Gate {
    /// Asked only when the verdict is [`Verdict::NeedsConfirmation`], after
    /// the plan has been shown. `true` means send.
    async fn confirm(&self, out: &Out, gated: &Gated) -> bool;
}

/// The console's answer: say how to consent, mark the run, and decline.
///
/// Not an error: the plan is correct and the user simply has not agreed to it
/// yet. Exit 1 so a script cannot mistake "waiting for consent" for "done",
/// but the message is an instruction rather than a complaint.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleGate;

impl Gate for ConsoleGate {
    async fn confirm(&self, out: &Out, gated: &Gated) -> bool {
        out.line(&spend::confirmation_message(&gated.estimate));
        out.set_exit(1);
        false
    }
}

/// Price the sweep, show it, and decide.
///
/// The order is fixed and is the observable contract: the plan table is printed
/// **before** the verdict is announced, so a refusal always comes with the
/// numbers that caused it rather than instead of them.
pub async fn gate<G: Gate>(
    out: &Out,
    gate: &G,
    config: &RouteConfig,
    survey: &Survey,
    prior: SizePrior,
) -> Decision {
    gate_titled(out, gate, config, survey, prior, "ROUTE PLAN", Stage::Final).await
}

/// The gate's arithmetic and its table, decided but not yet shown.
///
/// The console gate prints the plan and announces the verdict in a fixed
/// order; a full-screen UI shows the same plan in a modal and asks for the
/// same consent \[C53\]. Both consume this, so neither can price a sweep
/// differently from the other.
#[derive(Clone, Debug)]
pub struct Gated {
    pub estimate: Estimate,
    pub verdict: Verdict,
    /// The plan table, empty when the verdict is a radius refusal: the numbers
    /// would describe a sweep that is not going to happen, and the mistake is
    /// a typo, not a decision.
    pub plan: Vec<Block<'static>>,
}

impl Gated {
    /// The refusal text, when the verdict is one.
    #[must_use]
    pub fn refusal_message(&self, config: &RouteConfig) -> Option<String> {
        match &self.verdict {
            Verdict::Refused(refusal) => Some(spend::refusal_message(
                refusal,
                &self.estimate,
                config.radius_ly,
                config.max_requests,
            )),
            Verdict::Proceed | Verdict::NeedsConfirmation => None,
        }
    }
}

/// Price the sweep and draw its plan, printing nothing.
#[must_use]
pub fn decide(config: &RouteConfig, survey: &Survey, prior: SizePrior, title: &str) -> Gated {
    let estimate = Estimate::build(
        survey.counts,
        survey.exclusions.clone(),
        config.rate_per_second,
        &prior,
    );
    let verdict = spend::verdict(
        &estimate,
        config.radius_ly,
        config.max_requests,
        config.confirmed,
    );
    let plan = if matches!(verdict, Verdict::Refused(Refusal::RadiusTooWide)) {
        Vec::new()
    } else {
        views::route_plan(&PlanView {
            reference: &config.reference,
            radius_ly: config.radius_ly,
            complete_to_ly: survey.complete_to_ly,
            price_index: survey.price_index,
            ardent_requests: survey.ardent_requests,
            estimate: &estimate,
            rate_per_second: config.rate_per_second,
            max_requests: config.max_requests,
            prior,
            title,
        })
    };
    Gated {
        estimate,
        verdict,
        plan,
    }
}

/// The same gate under a caller's own heading, for a phase that is not the
/// last one.
pub async fn gate_titled<G: Gate>(
    out: &Out,
    gate: &G,
    config: &RouteConfig,
    survey: &Survey,
    prior: SizePrior,
    title: &str,
    stage: Stage,
) -> Decision {
    let gated = decide(config, survey, prior, title);

    // A radius past the ceiling is refused before the plan is drawn: the table
    // would be a page of numbers describing a sweep that is not going to
    // happen, and the mistake is a typo, not a decision.
    if let Verdict::Refused(refusal @ Refusal::RadiusTooWide) = gated.verdict {
        out.error_paragraph(&spend::refusal_message(
            &refusal,
            &gated.estimate,
            config.radius_ly,
            config.max_requests,
        ));
        out.set_exit(1);
        return Decision::Refused(refusal);
    }

    // `aside`, not `emit`: under `--json` stdout is one document and the plan
    // belongs on stderr \[C28\].
    out.aside(&gated.plan);

    match &gated.verdict {
        Verdict::Refused(refusal) => {
            out.error_paragraph(&spend::refusal_message(
                refusal,
                &gated.estimate,
                config.radius_ly,
                config.max_requests,
            ));
            out.set_exit(1);
            Decision::Refused(refusal.clone())
        }
        Verdict::NeedsConfirmation => {
            if gate.confirm(out, &gated).await {
                Decision::Sweep(gated.estimate)
            } else {
                Decision::Stopped(gated.estimate)
            }
        }
        Verdict::Proceed if config.dry_run => match stage {
            Stage::Final => Decision::Stopped(gated.estimate),
            Stage::Intermediate => Decision::Skipped(gated.estimate),
        },
        Verdict::Proceed => Decision::Sweep(gated.estimate),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;
    use edm_core::cli::access::{Cli, EnvSnapshot};
    use edm_core::cli::config::route_config;
    use edm_core::js::text::Metric;

    /// The real dispatch path, not a hand-built config: a gate tested against a
    /// struct nobody parses would not prove the flags reach it.
    fn config(argv: &[&str]) -> RouteConfig {
        // argv without the program name, exactly as `main` builds it.
        let owned: Vec<String> = argv.iter().map(|a| (*a).to_owned()).collect();
        let parsed = edm_core::cli::parse_dispatch(&owned);
        let parsed_route = parsed.route.expect("route must parse");
        let env = EnvSnapshot::empty();
        route_config(&Cli::new(&parsed_route, &env))
            .unwrap_or_else(|error| panic!("route config: {}", error.message()))
    }

    fn survey(systems: usize, markets: usize) -> Survey {
        Survey {
            complete_to_ly: 30.0,
            price_index: false,
            ardent_requests: 1,
            counts: spend::Counts {
                carriers_to_probe: 0,
                systems,
                systems_to_read: systems,
                stations_known: markets,
                markets_to_poll: markets,
                cached_fresh: 0,
            },
            exclusions: Vec::new(),
        }
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    fn run(argv: &[&str], survey: &Survey) -> (Decision, String) {
        let out = Out::capturing(200, Metric::Utf16, false);
        let decision = block_on(gate(
            &out,
            &ConsoleGate,
            &config(argv),
            survey,
            SizePrior::default(),
        ));
        (decision, out.captured())
    }

    /// The headline safeguard: over the ceiling, nothing proceeds.
    #[test]
    fn over_the_ceiling_nothing_proceeds() {
        let (decision, text) = run(
            &["route", "Sol", "--max-requests", "100"],
            &survey(200, 400),
        );
        assert_eq!(decision, Decision::Refused(Refusal::TooManyRequests));
        assert!(!decision.proceeds());
        assert!(text.contains("above the 100 ceiling"), "{text}");
        // And it says how to proceed, rather than only that one cannot.
        assert!(text.contains("--max-requests"), "{text}");
    }

    /// A refusal still shows the plan that caused it. A ceiling message with no
    /// numbers beside it cannot be acted on.
    #[test]
    fn a_refusal_shows_its_own_arithmetic() {
        let (_, text) = run(
            &["route", "Sol", "--max-requests", "100"],
            &survey(200, 400),
        );
        assert!(text.contains("ROUTE PLAN"), "{text}");
        assert!(
            text.contains("600  = 200 official batch + 400 market"),
            "{text}"
        );
    }

    /// And it is refused before a single Ardent query, not after the region has
    /// been enumerated. A radius past the ceiling is a fact about the argv:
    /// nothing learned about the region can make it acceptable. Measured live
    /// at `--radius 200`, where the run sat through nine anchor queries — over
    /// a minute — before being told the ceiling is 100.
    #[test]
    fn a_radius_past_the_ceiling_is_refused_before_any_work() {
        let over = config(&["route", "Sol", "--radius", "1200"]);
        assert_eq!(preflight(&over), Some(Refusal::RadiusTooWide));

        // And nothing at or under it is pre-refused, whatever else is wrong
        // with the command — those answers need the region.
        for argv in [
            vec!["route", "Sol", "--radius", "1000"],
            vec!["route", "Sol", "--radius", "500"],
            vec!["route", "Sol", "--radius", "200"],
            vec!["route", "Sol"],
            vec!["route", "Sol", "--max-requests", "1"],
        ] {
            assert_eq!(preflight(&config(&argv)), None, "{argv:?}");
        }
    }

    /// A radius past the hard ceiling is refused *without* drawing the table:
    /// the numbers would describe a sweep that is not going to happen.
    #[test]
    fn a_radius_past_the_ceiling_is_refused_before_the_table() {
        let (decision, text) = run(&["route", "Sol", "--radius", "1200"], &survey(2, 3));
        assert_eq!(decision, Decision::Refused(Refusal::RadiusTooWide));
        assert!(text.contains("exceeds the 1000 Ly ceiling"), "{text}");
        assert!(!text.contains("ROUTE PLAN"), "no table for a typo\n{text}");
    }

    /// Above the confirmation threshold the plan is shown and the sweep waits.
    #[test]
    fn a_large_sweep_waits_for_yes() {
        let (decision, text) = run(&["route", "Sol"], &survey(100, 300));
        assert!(matches!(decision, Decision::Stopped(_)), "{decision:?}");
        assert!(text.contains("pass --yes to send 400 requests"), "{text}");
        assert!(text.contains("ROUTE PLAN"), "{text}");
    }

    /// And with `--yes` the same sweep goes ahead.
    #[test]
    fn yes_unblocks_it() {
        let (decision, _) = run(&["route", "Sol", "--yes"], &survey(100, 300));
        assert!(decision.proceeds(), "{decision:?}");
    }

    /// Below the threshold no confirmation is asked for at all — the gate is a
    /// safeguard, not a ceremony.
    #[test]
    fn a_small_sweep_needs_no_confirmation() {
        let (decision, text) = run(&["route", "Sol"], &survey(4, 9));
        assert!(decision.proceeds(), "{decision:?}");
        assert!(!text.contains("--yes"), "{text}");
    }

    /// `--dry-run` prints the plan and stops, whatever the size.
    #[test]
    fn dry_run_stops_after_the_plan() {
        let (decision, text) = run(&["route", "Sol", "--dry-run"], &survey(4, 9));
        assert!(matches!(decision, Decision::Stopped(_)), "{decision:?}");
        assert!(text.contains("ROUTE PLAN"), "{text}");
    }

    /// An empty region is a legitimate answer, not a failure: the gate lets it
    /// through so the sweep can report zero markets and exit 0.
    #[test]
    fn a_sparse_region_proceeds_with_nothing_to_do() {
        let (decision, _) = run(&["route", "Skaudai"], &survey(0, 0));
        assert!(decision.proceeds(), "{decision:?}");
    }

    /// The decision a full-screen UI reads is the decision the console gate
    /// acts on, for every verdict the gate can reach \[C53\].
    #[test]
    fn decide_agrees_with_the_console_gate() {
        for (argv, survey) in [
            (vec!["route", "Sol", "--max-requests", "100"], survey(200, 400)),
            (vec!["route", "Sol", "--radius", "1200"], survey(2, 3)),
            (vec!["route", "Sol"], survey(100, 300)),
            (vec!["route", "Sol", "--yes"], survey(100, 300)),
            (vec!["route", "Sol"], survey(4, 9)),
            (vec!["route", "Sol", "--dry-run"], survey(4, 9)),
        ] {
            let config = config(&argv);
            let gated = decide(&config, &survey, SizePrior::default(), "ROUTE PLAN");
            let out = Out::capturing(200, Metric::Utf16, false);
            let decision = block_on(gate(&out, &ConsoleGate, &config, &survey, SizePrior::default()));
            let text = out.captured();
            match (&gated.verdict, &decision) {
                (Verdict::Refused(a), Decision::Refused(b)) => assert_eq!(a, b, "{argv:?}"),
                (Verdict::NeedsConfirmation, Decision::Stopped(_))
                | (Verdict::Proceed, Decision::Sweep(_) | Decision::Stopped(_)) => {}
                other => panic!("{argv:?}: {other:?}"),
            }
            assert_eq!(gated.estimate.requests, match &decision {
                Decision::Sweep(e) | Decision::Stopped(e) | Decision::Skipped(e) => e.requests,
                Decision::Refused(_) => gated.estimate.requests,
            });
            // The plan the UI would show is the plan the console showed.
            assert_eq!(gated.plan.is_empty(), !text.contains("ROUTE PLAN"), "{argv:?}\n{text}");
            if let Some(message) = gated.refusal_message(&config) {
                assert!(text.contains(&message), "{argv:?}\n{text}");
            }
        }
    }
}
