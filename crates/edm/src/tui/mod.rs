//! `edm ui` — the interactive front end \[C53\].
//!
//! The pipeline is the same code `edm route` and `edm sell` run; what differs
//! is where its words go and who decides when it runs. Words go through
//! [`Out::forwarding`] into the log pane. Decisions come from the keyboard,
//! read on a plain thread and handed over a channel to a loop on the same
//! single-threaded runtime every command uses, so the transport, the pacer
//! and the caches need no `Send` they never had.
//!
//! **No signal is handled.** Raw mode turns Ctrl-C into a key, which the loop
//! treats as quit; the terminal is put back by a guard's `Drop` and by a
//! panic hook that runs before the default one, so a panic's message lands on
//! a readable screen. crossterm's own SIGWINCH handler is the one signal the
//! binary now listens to, and only while this command runs \[R96\].

pub(crate) mod app;
pub(crate) mod autocomplete;
pub(crate) mod clip;
pub(crate) mod engine;
pub(crate) mod keys;
pub(crate) mod persist;
pub(crate) mod view;

use std::future::Future;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use edm_core::cli::access::{Cli, EnvSnapshot};
use edm_core::cli::parse::Args;
use edm_core::cli::ui::UiConfig;
use edm_core::js::text::Metric;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::cmd::{App, Overrides};
use crate::net::HttpTransport;
use crate::out::{Out, Stream};
use crate::ports::{Clock, Entropy, Fs, PinnedJitter, Ports, RealTimer};
use crate::route::cache::Cache;
use crate::route::pacer::Pacer;

use self::app::{AppState, Effect, Modal};
use self::engine::{Event, Session, ThreadEvent};

/// Raw mode and the alternate screen, undone on every way out.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("could not enter raw mode: {error}"))?;
        if let Err(error) = execute!(std::io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(format!("could not open the alternate screen: {error}"));
        }
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        Ok(Self)
    }
}

fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// The one timer: real sleeping, shared by the pacer and every job.
static TIMER: RealTimer = RealTimer;

/// What woke the loop.
enum Wake {
    Event(Event),
    Thread(ThreadEvent),
    JobDone,
    AuxDone,
    Tick,
    Closed,
}

type Job<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Run the interface until the user quits.
#[expect(
    clippy::too_many_lines,
    reason = "the loop: one select over every source, one match over every effect"
)]
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    args: &Args,
    env: &EnvSnapshot,
    http: &H,
    ports: &Ports<C, E, F>,
    out: &Out,
    overrides: &Overrides,
    config: &UiConfig,
) -> Result<(), String> {
    if !std::io::stdout().is_terminal() {
        return Err("edm ui needs a terminal on stdout; there is nothing to draw on".to_owned());
    }
    let cli = Cli::new(args, env);
    // Credentials are checked before the screen is taken, so a missing token
    // is reported where the shell can read it.
    let _ = App::open(Cli::new(args, env), http, ports, out, overrides)?;
    // The session's pacing and cache come from this command's own flags:
    // `--rps`, `--deadline`, `--cache-dir` mean here what they mean on route.
    let route_config = edm_core::cli::config::route_config_with_reference(&cli, Some("unused"))
        .map_err(|error| error.message().to_owned())?;
    let entropy = PinnedJitter {
        inner: &ports.entropy,
        unit: overrides.jitter.unwrap_or(f64::NAN),
    };
    let pacer = Pacer::new(
        crate::cmd::route::pacing(&route_config),
        &ports.clock,
        &TIMER,
        &entropy,
    );
    let cache_root = Cache::locate(
        cli.env("XDG_CACHE_HOME"),
        cli.env("HOME"),
        config.cache_dir.as_deref(),
    );
    let ui_dir = persist::directory(&cache_root);
    let pins_path = config
        .pins_file
        .as_deref()
        .map_or_else(|| ui_dir.join("pins.json"), PathBuf::from);
    let search_path = ui_dir.join("last-search.json");

    let (tx, rx) = async_channel::unbounded::<Event>();
    let (thread_tx, thread_rx) = async_channel::unbounded::<ThreadEvent>();
    let log_tx = tx.clone();
    let forward = Out::forwarding(
        200,
        Metric::Display,
        Box::new(move |stream: Stream, text: &str| {
            for line in text.lines() {
                let _ = log_tx.try_send(Event::Log {
                    stream,
                    line: line.to_owned(),
                });
            }
        }),
    );
    let session = Session {
        http,
        ports,
        env,
        overrides,
        out: &forward,
        timer: &TIMER,
        entropy: &entropy,
        pacer: &pacer,
        tx: tx.clone(),
        thread_tx: thread_tx.clone(),
        cache_root: cache_root.clone(),
    };

    let mut state = AppState::new(config.max_requests, config.refresh_seconds);
    state.now_ms = ports.clock.now_ms();
    match persist::read(&ports.fs, &pins_path).map(|text| persist::pins_from_json(&text)) {
        Some(Ok(pins)) => {
            let now = state.now_ms;
            state.pins = pins;
            for pin in &mut state.pins {
                pin.next_due_ms = now;
            }
        }
        Some(Err(message)) => state.log(Stream::Stderr, format!("pins: {message}; starting with none")),
        None => {}
    }
    if let Some(argv) = persist::read(&ports.fs, &search_path).and_then(|text| persist::last_search_from_json(&text)) {
        state.search.load(&argv);
    }

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| format!("could not start the terminal: {error}"))?;
    if let Ok(size) = crossterm::terminal::size() {
        state.size = size;
    }

    // Keys, on a thread of their own: `read` blocks, and the runtime is one
    // thread that must keep draining the pipeline's events. The thread ends
    // with the process; a blocked `read` cannot be joined and need not be.
    {
        let keys = thread_tx.clone();
        std::thread::Builder::new()
            .name("edm-input".to_owned())
            .spawn(move || {
                while let Ok(event) = crossterm::event::read() {
                    if keys.send_blocking(ThreadEvent::Input(event)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("could not start the input thread: {error}"))?;
    }
    let journal = engine::journal::spawn(
        crate::cmd::journal_candidates(&cli),
        Duration::from_secs_f64(edm_core::js::js_max(config.refresh_seconds / 2.0, 15.0)),
        thread_tx.clone(),
    )?;

    let mut job: Option<Job<'_>> = None;
    // The catalogue first; the nearby page follows the first journal read.
    state.jobs.aux = true;
    let mut aux: Option<Job<'_>> = Some(Box::pin(engine::run_aux(
        &session,
        engine::AuxSpec::Warmup { system: None },
    )));
    let mut solve_cancel = Arc::new(AtomicBool::new(false));
    let mut ticker = tokio::time::interval(Duration::from_millis(250));

    let mut running = true;
    while running {
        let wake = {
            let job_done = async {
                match job.as_mut() {
                    Some(future) => {
                        future.await;
                        Wake::JobDone
                    }
                    None => std::future::pending().await,
                }
            };
            let aux_done = async {
                match aux.as_mut() {
                    Some(future) => {
                        future.await;
                        Wake::AuxDone
                    }
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                received = rx.recv() => received.map_or(Wake::Closed, Wake::Event),
                received = thread_rx.recv() => received.map_or(Wake::Closed, Wake::Thread),
                woke = job_done => woke,
                woke = aux_done => woke,
                _ = ticker.tick() => Wake::Tick,
            }
        };
        let mut pending: Vec<Effect> = Vec::new();
        let now_ms = ports.clock.now_ms();
        match wake {
            Wake::Closed => break,
            Wake::Event(event) => pending.extend(state.reduce(event, now_ms)),
            Wake::Thread(event) => pending.extend(state.reduce(thread_event(event), now_ms)),
            Wake::JobDone => job = None,
            Wake::AuxDone => {
                aux = None;
                state.jobs.aux = false;
            }
            Wake::Tick => pending.extend(state.reduce(Event::Tick, now_ms)),
        }
        // Drain what else arrived before drawing once for the batch.
        while let Ok(event) = rx.try_recv() {
            pending.extend(state.reduce(event, now_ms));
        }
        while let Ok(event) = thread_rx.try_recv() {
            pending.extend(state.reduce(thread_event(event), now_ms));
        }
        for effect in pending {
            match effect {
                Effect::Quit => running = false,
                Effect::StartJob(spec) => {
                    if job.is_some() {
                        state.log(Stream::Stderr, format!("busy: {} waits", spec.label()));
                        continue;
                    }
                    state.jobs.active = Some(spec.label());
                    solve_cancel = Arc::new(AtomicBool::new(false));
                    job = Some(Box::pin(engine::run_job(&session, spec, solve_cancel.clone())));
                }
                Effect::StartAux(spec) => {
                    if aux.is_some() {
                        continue;
                    }
                    state.jobs.aux = true;
                    aux = Some(Box::pin(engine::run_aux(&session, spec)));
                }
                Effect::AnswerGate(answer) => {
                    if let Some(reply) = state.gate_reply.take() {
                        let _ = reply.try_send(answer);
                    }
                }
                Effect::CancelJob => {
                    if job.take().is_some() {
                        solve_cancel.store(true, Ordering::Relaxed);
                        state.log(Stream::Stderr, "cancelled");
                        state.reduce(Event::Stopped, now_ms);
                        state.search.status = Some("cancelled".to_owned());
                    }
                }
                Effect::SavePins => {
                    if let Err(message) = persist::write(&ports.fs, &pins_path, &persist::pins_json(&state.pins)) {
                        state.log(Stream::Stderr, format!("pins: {message}"));
                    }
                }
                Effect::SaveSearch(command) => {
                    if let Err(message) = persist::write(&ports.fs, &search_path, &persist::last_search_json(&command)) {
                        state.log(Stream::Stderr, format!("last search: {message}"));
                    }
                }
                Effect::Copy(text) => {
                    clip::copy(&text);
                    state.modal = Some(Modal::Copied(text));
                }
                Effect::ReadJournal => journal.read_now(),
            }
        }
        terminal
            .draw(|frame| view::draw(frame, &state))
            .map_err(|error| format!("could not draw: {error}"))?;
    }
    journal.stop();
    solve_cancel.store(true, Ordering::Relaxed);
    drop(job);
    drop(aux);
    Ok(())
}

fn thread_event(event: ThreadEvent) -> Event {
    match event {
        ThreadEvent::Input(input) => Event::Input(input),
        ThreadEvent::Journal(state) => Event::Journal(state),
        ThreadEvent::Solving(progress) => Event::Solving(progress),
    }
}
