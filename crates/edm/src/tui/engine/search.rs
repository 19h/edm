//! The searches and their rounds, as jobs \[C53\].
//!
//! Each job parses the argv the form built exactly as the command line would
//! — same table, same config reader, same defaults from the journal — opens
//! the same `App`, and calls the same `search` the command calls. The only
//! things that differ are who answers the gate and where the words go.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use edm_core::cli::Table;
use edm_core::cli::access::Cli;
use edm_core::cli::parse::{Args, parse_with};
use edm_core::domain::commander::CommanderState;

use crate::cmd::App;
use crate::cmd::route::quick::QuickSearch;
use crate::cmd::sell::SellSearch;
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};
use crate::route::plan;

use super::{Event, ModalGate, Session, ThreadSolver};

/// Parse an argv the way the extended commands do.
pub(crate) fn parse_argv(argv: &[String]) -> Result<Args, String> {
    parse_with(argv, Table::Extended).map_err(|error| error.to_string())
}

/// The journal, quietly: a search's defaults come from it, and its warnings
/// were shown when the reader thread first read it.
fn commander<H, C, E, F>(session: &Session<'_, H, C, E, F>, cli: &Cli<'_>) -> Option<CommanderState>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    crate::cmd::reload_commander_state(cli, session.ports)
}

/// Run the search `argv` describes. `Ok(true)` when it produced a result,
/// `Ok(false)` when it stopped at a gate.
#[expect(
    clippy::too_many_lines,
    reason = "the three searches and the refusals each makes before starting, in the commands' own words"
)]
pub(crate) async fn run_search<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    argv: Vec<String>,
    cancel: Arc<AtomicBool>,
) -> Result<bool, String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let parsed = parse_argv(&argv)?;
    let cli = Cli::new(&parsed, session.env);
    let gate = ModalGate { tx: &session.tx };
    match parsed.command.as_str() {
        "route" => {
            let commander = commander(session, &cli);
            let local_reference = commander
                .as_ref()
                .and_then(|state| state.current_system.as_ref())
                .map(|location| location.value.name.as_str());
            let mut config =
                edm_core::cli::config::route_config_with_reference(&cli, local_reference)
                    .map_err(|error| error.message().to_owned())?;
            if let Some(state) = &commander {
                crate::cmd::apply_commander_defaults(&cli, state, &mut config);
            }
            // The refusals `route::run` makes before any search, in its words.
            if let Some(refusal) = plan::preflight(&config) {
                return Err(edm_core::spend::refusal_message(
                    &refusal,
                    &edm_core::spend::Estimate::build(
                        edm_core::spend::Counts::default(),
                        Vec::new(),
                        config.rate_per_second,
                        &edm_core::spend::SizePrior::default(),
                    ),
                    config.radius_ly,
                    config.max_requests,
                ));
            }
            if config.fast_estimate {
                return Err("--fast-estimate is unavailable: it cannot safely estimate market IDs; omit it for an exact, spend-gated survey".to_owned());
            }
            if config.cargo == Some(0.0) {
                return Err("the hold has no capacity, so every leg would carry nothing. Pass --cargo <t> with the capacity you will be flying".to_owned());
            }
            if config.verify_systems {
                if config.quick.is_some() {
                    return Err("--verify-systems cannot be combined with --quick: the selected market ids are already polled live".to_owned());
                }
                if config.radius_ly > edm_core::consts::MARKETDATA_DISTANCE_LY_FALLBACK {
                    return Err("--verify-systems follows Frontier's 40 Ly marketdata policy; use --radius 40 or less".to_owned());
                }
            }
            let app = App::open(cli, session.http, session.ports, session.out, session.overrides)?;
            if config.quick.is_some() {
                let found = crate::cmd::route::quick::search(
                    &app,
                    &config,
                    commander.as_ref(),
                    session.timer,
                    session.pacer,
                    session.entropy,
                    &gate,
                )
                .await?;
                match found {
                    Some(search) => {
                        session
                            .send(Event::QuickDone {
                                search: Box::new(search),
                                argv,
                            })
                            .await;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            } else {
                let solver = ThreadSolver {
                    clock: &session.ports.clock,
                    thread_tx: session.thread_tx.clone(),
                    cancel,
                };
                let found = crate::cmd::route::survey(
                    &app,
                    &config,
                    commander.as_ref(),
                    session.timer,
                    session.pacer,
                    session.entropy,
                    &gate,
                    &solver,
                )
                .await?;
                match found {
                    Some(search) => {
                        session
                            .send(Event::SurveyDone {
                                search: Box::new(search),
                                argv,
                            })
                            .await;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
        }
        "sell" => {
            let commander = commander(session, &cli);
            let config = edm_core::cli::sell::sell_config(&cli)
                .map_err(|error| error.message().to_owned())?;
            let app = App::open(cli, session.http, session.ports, session.out, session.overrides)?;
            let found = crate::cmd::sell::search(
                &app,
                &config,
                commander.as_ref(),
                session.timer,
                session.pacer,
                session.entropy,
                &gate,
            )
            .await?;
            match found {
                Some(search) => {
                    session
                        .send(Event::SellDone {
                            search: Box::new(search),
                            argv,
                        })
                        .await;
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        other => Err(format!("`{other}` is not something this screen can run")),
    }
}

/// One round over a quick lookup's shortlist; the instance comes back with
/// the outcome.
pub(crate) async fn quick_round<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    mut search: Box<QuickSearch>,
    argv: Vec<String>,
    stop_if_moved: bool,
) -> Result<bool, String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let parsed = parse_argv(&argv)?;
    let cli = Cli::new(&parsed, session.env);
    let app = App::open(cli, session.http, session.ports, session.out, session.overrides)?;
    let outcome = crate::cmd::route::quick::round(
        &app,
        session.timer,
        session.pacer,
        session.entropy,
        &mut search,
        stop_if_moved,
    )
    .await?;
    session
        .send(Event::QuickRound {
            search,
            argv,
            outcome,
        })
        .await;
    Ok(true)
}

/// One round over a disposal.
pub(crate) async fn sell_round<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    mut search: Box<SellSearch>,
    argv: Vec<String>,
) -> Result<bool, String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let parsed = parse_argv(&argv)?;
    let cli = Cli::new(&parsed, session.env);
    let app = App::open(cli, session.http, session.ports, session.out, session.overrides)?;
    let outcome =
        crate::cmd::sell::round(&app, session.timer, session.pacer, session.entropy, &mut search)
            .await?;
    session
        .send(Event::SellRound {
            search,
            argv,
            outcome,
        })
        .await;
    Ok(true)
}
