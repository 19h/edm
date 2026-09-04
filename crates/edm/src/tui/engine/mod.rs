//! The pipeline, driven from a screen instead of a shell \[C53\].
//!
//! Nothing here prices, reads or ranks: those are `cmd::route::quick::search`,
//! `cmd::route::survey`, `cmd::sell::search` and their rounds, the same code
//! `edm route` and `edm sell` run. What this module adds is the plumbing a
//! screen needs around them — one event channel everything reports into, a
//! gate that asks a modal instead of stopping, a solver that runs on a thread
//! instead of freezing the loop, and jobs that own their inputs so the screen
//! can keep drawing while they run.

pub(crate) mod cards;
pub(crate) mod journal;
pub(crate) mod lookup;
pub(crate) mod pins;
pub(crate) mod search;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use edm_core::ardent::{NearbySystem, StationMatch};
use edm_core::cli::access::EnvSnapshot;
use edm_core::domain::commander::CommanderState;
use edm_route::pin::PinKey;

use crate::cmd::Overrides;
use crate::cmd::route::quick::{QuickSearch, RoundOutcome};
use crate::cmd::route::{Ranked, SolveRequest, Solver, SurveySearch};
use crate::cmd::sell::{SellRound, SellSearch};
use crate::net::HttpTransport;
use crate::out::{Out, Stream};
use crate::ports::{Clock, Entropy, Fs, PinnedJitter, Ports, RealTimer};
use crate::route::pacer::{Pacer, Spent};
use crate::route::plan::{Gate, Gated};

use self::pins::{PinJob, PinState};

/// One thing that happened, on the loop's own thread.
///
/// Not `Send`, and not meant to be: search results carry the solved instance
/// whole. What other threads have to say arrives as a [`ThreadEvent`].
pub(crate) enum Event {
    Input(crossterm::event::Event),
    /// The 250 ms clock, for countdowns and debouncing.
    Tick,
    /// One line the pipeline would have printed, with the stream it was
    /// bound for.
    Log { stream: Stream, line: String },
    /// A sweep needs consent. Answer on `reply`; dropping it declines.
    Gate {
        gated: Gated,
        reply: async_channel::Sender<bool>,
    },
    /// The survey solve said something.
    Solving(edm_route::watch::Event),
    /// A quick lookup finished. `argv` is what found it, so a round can
    /// rebuild the same command.
    QuickDone {
        search: Box<QuickSearch>,
        argv: Vec<String>,
    },
    SurveyDone {
        search: Box<SurveySearch>,
        argv: Vec<String>,
    },
    SellDone {
        search: Box<SellSearch>,
        argv: Vec<String>,
    },
    /// A results round finished; the instance comes back.
    QuickRound {
        search: Box<QuickSearch>,
        argv: Vec<String>,
        outcome: RoundOutcome,
    },
    SellRound {
        search: Box<SellSearch>,
        argv: Vec<String>,
        outcome: SellRound,
    },
    /// A pinned route was re-priced.
    Repriced { key: PinKey, state: Box<PinState> },
    Journal(Box<CommanderState>),
    /// Ardent's commodity catalogue, for completion.
    Catalogue(Vec<String>),
    /// The systems around the ship, for completion.
    Nearby(Vec<NearbySystem>),
    /// Stations whose names start with `query`.
    StationMatches {
        query: String,
        matches: Vec<StationMatch>,
    },
    /// The network job ended without a result: a gate declined, or a dry run.
    Stopped,
    /// The network job ended; `spent` is the session's running total.
    Finished { spent: Spent },
    /// The network job failed.
    Error(String),
    /// An aside (a lookup for completion) failed; worth a log line only.
    AuxError(String),
}

/// What another thread has to say.
#[derive(Debug)]
pub(crate) enum ThreadEvent {
    Input(crossterm::event::Event),
    Journal(Box<CommanderState>),
    Solving(edm_route::watch::Event),
}

/// Everything a job borrows for the session's lifetime.
///
/// One pacer for every job: one bucket, one breaker window, one `Spent`, so
/// `--max-requests` is a ceiling on the session and two jobs cannot double the
/// rate \[C37\]. One forwarding `Out`, so every word the pipeline says lands
/// in the log pane.
pub(crate) struct Session<'a, H, C, E, F> {
    pub http: &'a H,
    pub ports: &'a Ports<C, E, F>,
    pub env: &'a EnvSnapshot,
    pub overrides: &'a Overrides,
    pub out: &'a Out,
    pub timer: &'a RealTimer,
    pub entropy: &'a PinnedJitter<'a, E>,
    pub pacer: &'a Pacer<'a, C, RealTimer, PinnedJitter<'a, E>>,
    pub tx: async_channel::Sender<Event>,
    pub thread_tx: async_channel::Sender<ThreadEvent>,
    /// Where the price, access and atlas caches live.
    pub cache_root: PathBuf,
}

impl<H: HttpTransport, C: Clock, E: Entropy, F: Fs> Session<'_, H, C, E, F> {
    pub(crate) async fn send(&self, event: Event) {
        let _ = self.tx.send(event).await;
    }
}

/// The gate a screen answers: the plan goes up in a modal and the sweep waits
/// for a key.
pub(crate) struct ModalGate<'a> {
    pub tx: &'a async_channel::Sender<Event>,
}

impl Gate for ModalGate<'_> {
    async fn confirm(&self, _out: &Out, gated: &Gated) -> bool {
        let (reply, answer) = async_channel::bounded::<bool>(1);
        if self
            .tx
            .send(Event::Gate {
                gated: gated.clone(),
                reply,
            })
            .await
            .is_err()
        {
            return false;
        }
        // A dropped sender — the screen went away — is a no.
        answer.recv().await.unwrap_or(false)
    }
}

/// The survey solve on a thread of its own, so the screen keeps drawing.
///
/// The instance crosses as owned data and the ranking comes back the same
/// way; progress arrives as [`ThreadEvent::Solving`]. The deadline the run's
/// clock set is turned into an [`Instant`] here, because the thread has no
/// port clock and needs none.
pub(crate) struct ThreadSolver<'a, C: Clock> {
    pub clock: &'a C,
    pub thread_tx: async_channel::Sender<ThreadEvent>,
    pub cancel: Arc<AtomicBool>,
}

impl<C: Clock> Solver for ThreadSolver<'_, C> {
    async fn solve(&self, request: SolveRequest) -> Ranked {
        let (done, result) = async_channel::bounded::<Ranked>(1);
        let progress = self.thread_tx.clone();
        let cancel = self.cancel.clone();
        let remaining_ms = edm_core::js::js_max(request.deadline_ms - self.clock.now_ms(), 0.0);
        let deadline = Instant::now() + Duration::from_millis(remaining_ms as u64);
        let spawned = std::thread::Builder::new()
            .name("edm-solve".to_owned())
            .spawn(move || {
                let expired = || cancel.load(Ordering::Relaxed) || Instant::now() >= deadline;
                let sink = |event: edm_route::watch::Event| {
                    let _ = progress.try_send(ThreadEvent::Solving(event));
                };
                let watch = edm_route::watch::Watch::unlimited()
                    .until(&expired)
                    .reporting(&sink);
                let ranked = crate::cmd::route::solve_ranked(
                    &request.config,
                    request.listings,
                    &request.stations,
                    &request.candidate_demand_prices,
                    watch,
                );
                let _ = done.send_blocking(ranked);
            });
        if spawned.is_err() {
            return Ranked::empty();
        }
        result.recv().await.unwrap_or_else(|_| Ranked::empty())
    }
}

/// What the network slot can be asked to do. One at a time.
pub(crate) enum JobSpec {
    /// Run the search this argv describes: `route`, `route --quick`, `sell`.
    Search(Vec<String>),
    /// Re-price a quick lookup's shortlist \[C43\].
    QuickRound {
        search: Box<QuickSearch>,
        argv: Vec<String>,
        stop_if_moved: bool,
    },
    /// Re-plan a disposal \[C52\].
    SellRound {
        search: Box<SellSearch>,
        argv: Vec<String>,
    },
    /// Re-price one pinned route.
    Reprice(Box<PinJob>),
}

impl JobSpec {
    /// What the status bar calls it.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Search(argv) => format!("edm {}", argv.join(" ")),
            Self::QuickRound { .. } => "re-reading the shortlist".to_owned(),
            Self::SellRound { .. } => "re-planning the sale".to_owned(),
            Self::Reprice(job) => format!("re-pricing {}", job.label),
        }
    }
}

/// What an aside can be asked to do: free Ardent reads for completion, which
/// need no pacer and may run beside a network job.
pub(crate) enum AuxSpec {
    /// The commodity catalogue and the systems around `system`.
    Warmup { system: Option<String> },
    StationSearch(String),
}

/// Run one network job to completion, reporting into the session's channel.
pub(crate) async fn run_job<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    spec: JobSpec,
    cancel: Arc<AtomicBool>,
) where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let outcome = match spec {
        JobSpec::Search(argv) => search::run_search(session, argv, cancel).await,
        JobSpec::QuickRound {
            search,
            argv,
            stop_if_moved,
        } => search::quick_round(session, search, argv, stop_if_moved).await,
        JobSpec::SellRound { search, argv } => search::sell_round(session, search, argv).await,
        JobSpec::Reprice(job) => pins::reprice(session, *job).await,
    };
    match outcome {
        Ok(true) => {
            session
                .send(Event::Finished {
                    spent: session.pacer.spent(),
                })
                .await;
        }
        Ok(false) => session.send(Event::Stopped).await,
        Err(message) => session.send(Event::Error(message)).await,
    }
}

/// Run one aside.
pub(crate) async fn run_aux<H, C, E, F>(session: &Session<'_, H, C, E, F>, spec: AuxSpec)
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let outcome = match spec {
        AuxSpec::Warmup { system } => lookup::warmup(session, system.as_deref()).await,
        AuxSpec::StationSearch(prefix) => lookup::station_search(session, prefix).await,
    };
    if let Err(message) = outcome {
        session.send(Event::AuxError(message)).await;
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod send_bounds {
    use super::*;

    /// The solver thread hands the ranking back across a channel, which needs
    /// the instance to be `Send` — a fact worth pinning, because one `Rc` in a
    /// market row would turn a compile error into a design change.
    #[test]
    fn a_solved_instance_can_cross_a_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Ranked>();
        assert_send::<SolveRequest>();
        assert_send::<CommanderState>();
        assert_send::<ThreadEvent>();
    }
}
