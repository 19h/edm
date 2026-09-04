//! The job protocol, driven headlessly over a scripted transport.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use edm_core::cli::Table;
use edm_core::cli::access::{Cli, EnvSnapshot};
use edm_core::cli::parse::parse_with;
use edm_core::js::text::Metric;
use edm_core::wire::{Nonce, seal_query};

use crate::cmd::Overrides;
use crate::net::{HeaderView, HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::out::Out;
use crate::ports::{CountingEntropy, FixedClock, PinnedJitter, Ports, RealTimer, RecordingFs};
use crate::route::pacer::Pacer;

use super::{Event, JobSpec, Session, run_job};

type Reply = Result<HttpResponse, TransportError>;

/// The scripted transport the integration tests use, in miniature: the first
/// route whose needle the URL contains answers, one reply per request.
struct FakeHttp {
    routes: RefCell<Vec<(&'static str, VecDeque<Reply>)>>,
    calls: RefCell<Vec<String>>,
}

impl FakeHttp {
    fn new() -> Self {
        Self {
            routes: RefCell::new(Vec::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn route(self, needle: &'static str, replies: Vec<Reply>) -> Self {
        self.routes.borrow_mut().push((needle, replies.into()));
        self
    }
}

impl HttpTransport for FakeHttp {
    async fn send(&self, request: HttpRequest<'_>) -> Reply {
        let target = request
            .url
            .split_once('?')
            .map_or(request.url, |(head, _)| head);
        self.calls.borrow_mut().push(target.to_owned());
        let mut routes = self.routes.borrow_mut();
        for (needle, replies) in routes.iter_mut() {
            if request.url.contains(*needle)
                && let Some(reply) = replies.pop_front()
            {
                return reply;
            }
        }
        Err(TransportError::Other(format!("no scripted reply for {target}")))
    }
}

#[expect(clippy::unnecessary_wraps, reason = "a script is a Vec<Reply>, and a transport failure is one of the things it can hold")]
fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Reply {
    Ok(HttpResponse {
        status,
        status_text: String::new(),
        headers: HeaderView::from_pairs(
            headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        ),
        body: body.to_owned(),
    })
}

fn json(body: &str) -> Reply {
    reply(200, &[("content-type", "application/json")], body)
}

const RESPONSE_NONCE: &str = "0123456789ab";

/// An LZ4 block that is one run of literals and no matches.
fn lz4_literals(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    if data.len() < 15 {
        out.push((data.len() as u8) << 4);
    } else {
        out.push(0xF0);
        let mut remaining = data.len() - 15;
        while remaining >= 255 {
            out.push(255);
            remaining -= 255;
        }
        out.push(remaining as u8);
    }
    out.extend_from_slice(data);
    out
}

/// A game-internal API 2xx: LZ4-framed, sealed, base64.
fn sealed(document: &str) -> Reply {
    let nonce = Nonce::parse_arg(RESPONSE_NONCE).expect("twelve hex characters");
    let mut framed = b"EDDE".to_vec();
    framed.extend_from_slice(&[0, 0, 0, 0]);
    framed.extend_from_slice(&lz4_literals(document.as_bytes()));
    reply(
        200,
        &[
            ("nonce", RESPONSE_NONCE),
            ("uncompressedsize", &document.len().to_string()),
            ("content-type", "application/octet-stream"),
        ],
        &seal_query(&framed, &nonce),
    )
}

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;
const EXPORTS: &str = r#"[
  {"commodityName":"gold","marketId":128016384,"stationName":"Galileo","stationType":"Ocellus","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":9000,"stock":5000}
]"#;
const IMPORTS: &str = r#"[
  {"commodityName":"gold","marketId":128016576,"stationName":"Titan City","stationType":"Coriolis","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"sellPrice":11500,"demand":7000,"demandBracket":3}
]"#;
const LOCAL: &str = "[]";

fn quick_http() -> FakeHttp {
    FakeHttp::new()
        .route("/v2/system/name/Sol", vec![json(SOL)])
        .route("/v2/commodities", vec![json(r#"[{"commodityName":"gold"}]"#)])
        .route("/commodity/name/gold/nearby/exports", vec![json(EXPORTS)])
        .route("/commodity/name/gold/nearby/imports", vec![json(IMPORTS)])
        .route("/v2/system/name/Sol/commodity/name/gold", vec![json(LOCAL)])
        .route(
            "/2.0/elite/market/list",
            vec![
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-source.json")),
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-sink.json")),
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-source.json")),
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-sink.json")),
            ],
        )
}

fn env() -> EnvSnapshot {
    EnvSnapshot::from_pairs(vec![
        ("COMMANDER_ID".to_owned(), "F1234567".to_owned()),
        ("MACHINE_ID".to_owned(), "machine-1".to_owned()),
        ("MACHINE_TOKEN".to_owned(), "m".repeat(80)),
        ("AUTH_TOKEN".to_owned(), "a".repeat(2024)),
        ("EDM_JITTER".to_owned(), "0".to_owned()),
    ])
}

fn kind(event: &Event) -> &'static str {
    match event {
        Event::Input(_) => "input",
        Event::Tick => "tick",
        Event::Log { .. } => "log",
        Event::Gate { .. } => "gate",
        Event::Solving(_) => "solving",
        Event::QuickDone { .. } => "quick-done",
        Event::SurveyDone { .. } => "survey-done",
        Event::SellDone { .. } => "sell-done",
        Event::QuickRound { .. } => "quick-round",
        Event::SellRound { .. } => "sell-round",
        Event::Repriced { .. } => "repriced",
        Event::Journal(_) => "journal",
        Event::Catalogue(_) => "catalogue",
        Event::Nearby(_) => "nearby",
        Event::StationMatches { .. } => "stations",
        Event::Stopped => "stopped",
        Event::Finished { .. } => "finished",
        Event::Error(_) => "error",
        Event::AuxError(_) => "aux-error",
    }
}

static TIMER: RealTimer = RealTimer;

/// A quick lookup run as a job reports its ranking and then finishes, having
/// read exactly the markets the command would have.
#[test]
fn a_quick_search_job_reports_its_ranking_and_finishes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        tokio::time::pause();
        let http = quick_http();
        let env = env();
        let overrides = Overrides::from_env(&env);
        let ports = Ports {
            clock: FixedClock {
                now_ms: 1_700_000_000_000.0,
                uptime_seconds: 12_345.0,
            },
            entropy: CountingEntropy::default(),
            fs: RecordingFs(RefCell::new(Vec::new())),
        };
        let session_argv = vec!["ui".to_owned(), "--rps".to_owned(), "100".to_owned()];
        let ui_args = parse_with(&session_argv, Table::Extended).expect("ui parses");
        let cli = Cli::new(&ui_args, &env);
        let route_config = edm_core::cli::config::route_config_with_reference(&cli, Some("unused"))
            .expect("a route config");
        let entropy = PinnedJitter {
            inner: &ports.entropy,
            unit: 0.0,
        };
        let pacer = Pacer::new(
            crate::cmd::route::pacing(&route_config),
            &ports.clock,
            &TIMER,
            &entropy,
        );
        let (tx, rx) = async_channel::unbounded::<Event>();
        let (thread_tx, _thread_rx) = async_channel::unbounded();
        let said = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
        let log = said.clone();
        let out = Out::forwarding(
            200,
            Metric::Display,
            Box::new(move |_, text| log.borrow_mut().push(text.to_owned())),
        );
        let session = Session {
            http: &http,
            ports: &ports,
            env: &env,
            overrides: &overrides,
            out: &out,
            timer: &TIMER,
            entropy: &entropy,
            pacer: &pacer,
            tx,
            thread_tx,
            cache_root: std::path::PathBuf::from("/cache/route"),
        };
        let argv: Vec<String> = [
            "route", "Sol", "--quick", "1", "--item", "gold", "--qty", "100", "--cargo", "784",
            "--shape", "one-way", "--no-cache", "--rps", "100",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        run_job(&session, JobSpec::Search(argv), Arc::new(AtomicBool::new(false))).await;

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let kinds: Vec<&str> = events.iter().map(kind).collect();
        assert_eq!(
            kinds,
            ["quick-done", "finished"],
            "words go through the forwarding Out, results through the channel: {kinds:?}\n{}",
            said.borrow().join("")
        );
        let Event::QuickDone { search, argv } = &events[0] else {
            unreachable!()
        };
        assert_eq!(argv[0], "route");
        assert_eq!(search.ranked.routes().len(), 1, "{}", said.borrow().join(""));
        assert!(said.borrow().iter().any(|line| line.contains("QUICK LOOKUP")));
        assert_eq!(
            http.calls
                .borrow()
                .iter()
                .filter(|call| call.contains("/2.0/elite/market/list"))
                .count(),
            2,
            "{:?}",
            http.calls.borrow()
        );
    });
}

/// A pinned route is re-priced by reading exactly its own markets, live, even
/// when the cache is warm — the write-side cache is refresh-mode, so an entry
/// the last read wrote cannot answer for this one \[C38\].
#[test]
#[expect(clippy::too_many_lines, reason = "one session, one pin, one re-price, checked end to end")]
fn a_reprice_reads_exactly_the_pins_markets_live() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        tokio::time::pause();
        let http = FakeHttp::new().route(
            "/2.0/elite/market/list",
            vec![
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-source.json")),
                sealed(include_str!("../../../../../xtask/scenarios/payloads/market-gold-sink.json")),
            ],
        );
        let env = env();
        let overrides = Overrides::from_env(&env);
        let ports = Ports {
            clock: FixedClock {
                now_ms: 1_700_000_000_000.0,
                uptime_seconds: 12_345.0,
            },
            entropy: CountingEntropy::default(),
            fs: RecordingFs(RefCell::new(Vec::new())),
        };
        let session_argv = vec!["ui".to_owned(), "--rps".to_owned(), "100".to_owned()];
        let ui_args = parse_with(&session_argv, Table::Extended).expect("ui parses");
        let cli = Cli::new(&ui_args, &env);
        let route_config = edm_core::cli::config::route_config_with_reference(&cli, Some("unused"))
            .expect("a route config");
        let entropy = PinnedJitter {
            inner: &ports.entropy,
            unit: 0.0,
        };
        let pacer = Pacer::new(
            crate::cmd::route::pacing(&route_config),
            &ports.clock,
            &TIMER,
            &entropy,
        );
        let (tx, rx) = async_channel::unbounded::<Event>();
        let (thread_tx, _thread_rx) = async_channel::unbounded();
        let out = Out::forwarding(200, Metric::Display, Box::new(|_, _| {}));
        let session = Session {
            http: &http,
            ports: &ports,
            env: &env,
            overrides: &overrides,
            out: &out,
            timer: &TIMER,
            entropy: &entropy,
            pacer: &pacer,
            tx,
            thread_tx,
            cache_root: std::path::PathBuf::from("/cache/route"),
        };
        let station = |id: f64, name: &str, kind: &str| edm_core::ardent::ArdentStation {
            market_id: id,
            station_name: name.to_owned(),
            system_name: "Sol".to_owned(),
            system_address: 10_477_373_803.0,
            station_type: Some(kind.to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(505.0),
            coordinates: edm_core::domain::id64::Coordinates { x: 0.0, y: 0.0, z: 0.0 },
        };
        let job = super::pins::PinJob {
            key: edm_route::pin::PinKey {
                kind: edm_route::pin::PinKind::OneWay,
                stations: vec![128_016_384, 128_016_576],
                commodities: vec!["Gold".to_owned()],
            },
            label: "Galileo > Titan City (Gold)".to_owned(),
            argv: ["route", "Sol", "--quick", "1", "--item", "gold", "--cargo", "784", "--rps", "100"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            stations: vec![
                station(128_016_384.0, "Galileo", "Ocellus"),
                station(128_016_576.0, "Titan City", "Coriolis"),
            ],
            commander: None,
        };
        run_job(&session, JobSpec::Reprice(Box::new(job)), Arc::new(AtomicBool::new(false))).await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        let kinds: Vec<&str> = events.iter().map(kind).collect();
        assert_eq!(kinds, ["repriced", "finished"]);
        let Event::Repriced { state, .. } = &events[0] else {
            unreachable!()
        };
        let card = state.route.as_ref().expect("the route still trades");
        assert_eq!(card.legs.len(), 1);
        assert_eq!(card.legs[0].from, "Galileo");
        assert_eq!(card.legs[0].to, "Titan City");
        assert!(card.profit > 0);
        assert_eq!(state.markets.len(), 2);
        assert_eq!(state.markets[0].status, "read live");
        assert!(state.markets[0].rows.iter().any(|row| row.name == "Gold"));
        assert_eq!(state.requests, 2);
        // The cache was written by the read, and a second re-price still
        // reads both markets live.
        let cached = ports
            .fs
            .0
            .borrow()
            .iter()
            .filter(|(path, _)| path.to_string_lossy().contains("frontier-market-list"))
            .count();
        assert_eq!(cached, 2, "both listings were written to the cache");
        assert_eq!(
            http.calls
                .borrow()
                .iter()
                .filter(|call| call.contains("/2.0/elite/market/list"))
                .count(),
            2
        );
    });
}
