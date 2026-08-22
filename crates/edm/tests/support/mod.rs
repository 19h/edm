//! Driving a whole command with no network, no clock and no terminal.
//!
//! Three pieces. [`FakeHttp`] replays scripted replies keyed on a URL fragment,
//! because the market id a request carries is inside the encrypted query and
//! cannot be routed on. [`drive`] wires that to the [`FixedClock`],
//! [`CountingEntropy`] and [`RecordingFs`] the port already exposes for the
//! parity harness, so `fTime`, every nonce and every dump are reproducible.
//! And [`captured`] redirects file descriptor 1.
//!
//! **Why each test binary here holds exactly one `#[test]`.** `Out` writes to
//! the real stdout rather than through `print!`, so the only way to read its
//! output back is to redirect the descriptor — and a descriptor is
//! process-wide. libtest reports each finished test on that same descriptor
//! from its main thread, so a second test finishing while the first holds the
//! redirect writes `test … ok` into the middle of the captured output. One test
//! per binary is what makes the capture deterministic; the scenarios inside it
//! are separated by named `insta` snapshots instead.

#![allow(
    dead_code,
    reason = "one support module shared by several test binaries"
)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use edm::cmd::{self, Overrides};
use edm::net::{HeaderView, HttpRequest, HttpResponse, HttpTransport, TransportError};
use edm::out::Out;
use edm::ports::{CountingEntropy, FixedClock, Ports, RecordingFs};
use edm_core::cli::{self, EnvSnapshot};
use edm_core::wire::{Nonce, seal_query};

/// The width a non-terminal stdout gets, and the width every snapshot here is
/// taken at.
pub(crate) const WIDTH: usize = 100;

/// The nonce every scripted reply carries. Lowercase, so the request and
/// response nonce constructors cannot disagree about case \[R57\].
pub(crate) const RESPONSE_NONCE: &str = "0123456789ab";

// ---------------------------------------------------------------------------
// A scripted transport
// ---------------------------------------------------------------------------

pub(crate) type Reply = Result<HttpResponse, TransportError>;

/// One route's replies, handed out in request order.
struct Route {
    /// Matched against the request URL as a substring; the first route that
    /// still has a reply takes the request.
    needle: &'static str,
    replies: VecDeque<Reply>,
}

#[derive(Default)]
pub(crate) struct FakeHttp {
    routes: RefCell<Vec<Route>>,
    /// `METHOD url` for every request, query stripped — the wire trace a test
    /// can assert on without unsealing anything.
    calls: RefCell<Vec<String>>,
}

impl FakeHttp {
    #[must_use]
    pub(crate) fn route(mut self, needle: &'static str, replies: Vec<Reply>) -> Self {
        self.routes.get_mut().push(Route {
            needle,
            replies: replies.into(),
        });
        self
    }
}

impl HttpTransport for FakeHttp {
    async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
        // The query is a kilobyte of ciphertext whose nonce changes per
        // request, so it is useless in a trace.
        let target = request
            .url
            .split_once('?')
            .map_or(request.url, |(head, _)| head);
        self.calls
            .borrow_mut()
            .push(format!("{} {target}", request.method));

        let mut routes = self.routes.borrow_mut();
        for route in routes.iter_mut() {
            if request.url.contains(route.needle)
                && let Some(reply) = route.replies.pop_front()
            {
                return reply;
            }
        }
        Err(TransportError::Other(format!(
            "no scripted reply for {target}"
        )))
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a script is a Vec<Reply>, and a transport failure is one of the things it can hold"
)]
pub(crate) fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Reply {
    Ok(HttpResponse {
        status,
        status_text: http::StatusCode::from_u16(status)
            .ok()
            .and_then(|code| code.canonical_reason())
            .unwrap_or("")
            .to_owned(),
        headers: HeaderView::from_pairs(
            headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        ),
        body: body.to_owned(),
    })
}

/// A plain JSON reply, for Ardent and EDDN.
pub(crate) fn json_reply(body: &str) -> Reply {
    reply(200, &[("content-type", "application/json")], body)
}

/// A game-internal API 2xx: LZ4-framed, `ChaCha20`-sealed, base64.
pub(crate) fn sealed(document: &str) -> Reply {
    let nonce = Nonce::parse_arg(RESPONSE_NONCE).expect("twelve hex characters");
    let mut framed = b"EDDE".to_vec();
    // Bytes 4..8 are never inspected by the frame check \[R60\].
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

/// An LZ4 block that is one run of literals and no matches.
///
/// The decompressor stops when the source is exhausted, so this is a legal
/// encoding of any input — which is all a fixture needs, and it keeps a
/// compressor out of the test build.
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

// ---------------------------------------------------------------------------
// Running a command
// ---------------------------------------------------------------------------

pub(crate) struct Run {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit: ExitCode,
    /// What `--dump` wrote, as [`RecordingFs`] saw it.
    pub(crate) files: Vec<(PathBuf, String)>,
    /// `METHOD url`, in order.
    pub(crate) calls: Vec<String>,
}

impl Run {
    /// `ExitCode` exposes no accessor, so it is compared through `Debug` — the
    /// idiom `out.rs`'s own test already uses.
    #[track_caller]
    pub(crate) fn assert_exit(&self, code: u8) {
        assert_eq!(
            format!("{:?}", self.exit),
            format!("{:?}", ExitCode::from(code)),
            "exit code\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout,
            self.stderr
        );
    }
}

/// The four credentials every command loads, including `markets --dry-run`
/// \[R50\].
fn credentials() -> Vec<(String, String)> {
    vec![
        ("COMMANDER_ID".to_owned(), "F1234567".to_owned()),
        ("MACHINE_ID".to_owned(), "machine-1".to_owned()),
        ("MACHINE_TOKEN".to_owned(), "m".repeat(80)),
        ("AUTH_TOKEN".to_owned(), "a".repeat(2024)),
    ]
}

pub(crate) fn drive(argv: &[&str], http: &FakeHttp) -> Run {
    drive_with_env(argv, http, Vec::new())
}

pub(crate) fn drive_with_env(argv: &[&str], http: &FakeHttp, extra: Vec<(String, String)>) -> Run {
    drive_with_env_and_files(argv, http, extra, Vec::new())
}

pub(crate) fn drive_with_env_and_files(
    argv: &[&str],
    http: &FakeHttp,
    extra: Vec<(String, String)>,
    files: Vec<(PathBuf, String)>,
) -> Run {
    let argv: Vec<String> = argv.iter().map(|token| (*token).to_owned()).collect();
    // `EnvSnapshot` is first-wins per name \[R55\], so the caller's overrides go
    // in front of the defaults.
    let mut pairs = extra;
    pairs.extend(credentials());
    let env = EnvSnapshot::from_pairs(pairs);
    let overrides = Overrides::from_env(&env);

    let ports = Ports {
        // A fixed instant, so `fTime`, `requestTime` and every EDDN timestamp
        // are the same on every run.
        clock: FixedClock {
            now_ms: 1_700_000_000_000.0,
            uptime_seconds: 12_345.0,
        },
        entropy: CountingEntropy::default(),
        fs: RecordingFs(RefCell::new(files)),
    };

    let parsed = cli::parse_dispatch(&argv);
    let out = Out::new(WIDTH, overrides.metric, cmd::wants_json(&parsed));

    let (exit, stdout, stderr) = captured(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(cmd::run(parsed, &env, http, &ports, &out, &overrides));
        out.flush();
        out.exit_code()
    });

    Run {
        stdout,
        stderr,
        exit,
        files: ports.fs.0.borrow().clone(),
        calls: http.calls.borrow().clone(),
    }
}

/// Runs `work` with descriptors 1 and 2 pointing at scratch files, and returns
/// what they collected.
pub(crate) fn captured<T>(work: impl FnOnce() -> T) -> (T, String, String) {
    let id = std::process::id();
    let out_path = std::env::temp_dir().join(format!("edm-stdout-{id}"));
    let err_path = std::env::temp_dir().join(format!("edm-stderr-{id}"));
    let out_file = std::fs::File::create(&out_path).expect("a scratch file for stdout");
    let err_file = std::fs::File::create(&err_path).expect("a scratch file for stderr");

    let saved_out = rustix::io::dup(rustix::stdio::stdout()).expect("dup stdout");
    let saved_err = rustix::io::dup(rustix::stdio::stderr()).expect("dup stderr");
    rustix::stdio::dup2_stdout(&out_file).expect("redirect stdout");
    rustix::stdio::dup2_stderr(&err_file).expect("redirect stderr");

    // A panic behind a redirected descriptor would swallow its own message, so
    // the streams go back before it is allowed to continue.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    rustix::stdio::dup2_stdout(&saved_out).expect("restore stdout");
    rustix::stdio::dup2_stderr(&saved_err).expect("restore stderr");

    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);

    match result {
        Ok(value) => (value, stdout, stderr),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A market listing: three commodities across two categories, `gold_held` units
/// of gold in the hold, and a balance the affordability clamp can bite on.
#[must_use]
pub(crate) fn listing(gold_held: u32) -> String {
    format!(
        r#"{{"id":4306502403,"credits":1000000,"debt":0,"allowsDumping":true,
"commodities":{{
"128049204":{{"id":128049204,"name":"Gold","categoryname":"Metals","stock":120,"stockBracket":2,"buyPrice":9000,"sellPrice":8900,"fencePrice":0,"demand":0,"demandBracket":0,"meanPrice":9500,"consumer":0,"producer":1,"rare":0,"legality":""}},
"128049205":{{"id":128049205,"name":"Silver","categoryname":"Metals","stock":40,"stockBracket":1,"buyPrice":4600,"sellPrice":4550,"fencePrice":0,"demand":0,"demandBracket":0,"meanPrice":4800,"consumer":0,"producer":1,"rare":0,"legality":""}},
"128049669":{{"id":128049669,"name":"Biowaste","categoryname":"Waste","stock":0,"stockBracket":0,"buyPrice":0,"sellPrice":60,"fencePrice":0,"demand":900,"demandBracket":3,"meanPrice":65,"consumer":1,"producer":0,"rare":0,"legality":""}}
}},
"inventory":[{{"commodity":"Gold","qty":{gold_held},"value":90000,"stolen":false,"marked":0,"owner":0,"origin":0}}]}}"#
    )
}

/// A body that decrypts cleanly and is not a market listing — the case that
/// prints a PAYLOAD block instead of tables, and the one that lets a sweep fail
/// a market without any non-2xx setting the exit code first.
pub(crate) const NOT_A_LISTING: &str = r#"{"errors":["Market not found"]}"#;

/// `/2.0/elite/starsystem` for a system with seven dockable locations: five
/// that trade, one fleet carrier, and one with no commodity market.
#[must_use]
pub(crate) fn starsystem() -> String {
    r#"{"starsystem":{
"starsystem":{"minorFactions":{"1234":{"name":"Jaques"}}},
"polities":{"0":{"controllingMinorFaction":1234,"markets":{
"3229009408":{"id":3229009408,"name":"Jaques Station","poiType":"Starport","distFromSystem":286,"bodyName":"Colonia 2","imported":{"a":1,"b":1},"exported":{"c":1},"services":{"commodities":"ok","blackmarket":"ok","refuel":"ok"},"economies":{"0":{"name":"Tourism","proportion":1}}},
"3229009409":{"id":3229009409,"name":"Ohm City","poiType":"Outpost","distFromSystem":417,"bodyName":"Colonia 5","imported":{"a":1},"exported":{"b":1},"services":{"commodities":"ok"},"economies":{"0":{"name":"Industrial","proportion":1}}},
"3229009410":{"id":3229009410,"name":"Kinsey Orbital","poiType":"Starport","distFromSystem":1200,"imported":{"a":1},"exported":{"b":1,"c":1},"services":{"commodities":"ok","outfitting":"ok"}},
"3229009411":{"id":3229009411,"name":"Colonia Hub","poiType":"DockablePlanetStation","distFromSystem":90,"imported":{"a":1},"exported":{"b":1},"services":{"commodities":"ok"}},
"3229009412":{"id":3229009412,"name":"Centauri Depot","poiType":"Outpost","distFromSystem":33,"imported":{"a":1},"exported":{"b":1},"services":{"commodities":"ok"}},
"3229009413":{"id":3229009413,"name":"Idle Beacon","poiType":"Outpost","distFromSystem":12,"services":{"refuel":"ok"}},
"3229009414":{"id":3229009414,"name":"K3M-B4G","poiType":"FleetCarrier","distFromSystem":5,"imported":{"a":1},"exported":{"b":1},"services":{"commodities":"ok"}}
}}}
}}"#
    .to_owned()
}

/// Ardent's answer for Colonia, whose address and coordinates round-trip
/// through the ID64 codec.
pub(crate) const ARDENT_COLONIA: &str = r#"{"systemName":"Colonia","systemAddress":3238296097059,"systemX":-9530.5,"systemY":-910.28125,"systemZ":19808.125}"#;

/// The game-internal API origin every fixture URL is built from.
pub(crate) const GAME_INTERNAL_API: &str = "https://api.orerve.net";
