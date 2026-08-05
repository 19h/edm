//! `edm-mock` — the Frontier, Ardent and EDDN servers, on one port.
//!
//! Hand-rolled on `tokio::net::TcpListener` rather than built on hyper, and the
//! reason is the diff. The harness compares *raw* bytes: reason phrases, header
//! order, and duplicate headers all have to be controllable, and hyper
//! normalises every one of them — it will not emit two `uncompressedsize`
//! headers, which is exactly the fixture that pins **[R71]**. A server that
//! cannot produce the malformed cases cannot prove the client handles them.
//!
//! One port serves three roles, routed by path prefix, because the scenarios
//! need to watch a Companion API poll and an EDDN post interleave inside a
//! single sweep.
//!
//! Every request is recorded to a wire log in plain text. Plain text and not
//! JSON: a serializer on this side of the comparison could normalise away the
//! very thing being compared, and **[F2]** says only one serializer is allowed
//! to touch a payload.

use std::fmt::Write as _;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::codec;
use crate::scenario::{Profile, Reply, Route, Scenario};

/// Headers the HTTP client injects that neither implementation controls.
///
/// Presence is asserted — an injected header that stops being sent is a real
/// change — but the value is not compared. **[C20]** registers the divergence
/// for `accept-encoding`, whose value reqwest composes from its own feature
/// set, and **[C19]** for the Ardent/EDDN `user-agent`, which is `edm/1.0.0`
/// where Bun would send `Bun/x.y`. `content-length` is deliberately *not* here:
/// **[R66]** turns on it.
fn injected(profile: Profile, name: &str) -> Injected {
    match name {
        "host" | "accept-encoding" => Injected::PresenceOnly,
        // Keep-alive is the HTTP/1.1 default, so whether a client states it is
        // a property of the client and not of this port — the same class of
        // artefact as the header ordering C20 already normalises.
        "connection" => Injected::Ignored,
        "user-agent" if profile != Profile::Frontier => Injected::PresenceOnly,
        _ => Injected::Diffed,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Injected {
    Diffed,
    PresenceOnly,
    Ignored,
}

/// One recorded request.
#[derive(Clone, Debug)]
struct Record {
    profile: Profile,
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct RouteState {
    route: Route,
    served: usize,
}

#[derive(Default)]
struct State {
    routes: Vec<RouteState>,
    log: Vec<Record>,
    /// Things that should not have happened: an unrouted path, a route asked
    /// for one more reply than it has. Reported alongside the diff rather than
    /// silently answered, because either one means the scenario is wrong.
    problems: Vec<String>,
    /// Bumped on every reset; a connection parked on a `never` reply notices
    /// and lets go.
    generation: u64,
}

pub(crate) struct Mock {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Mock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mock").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl Mock {
    /// Binds an ephemeral port and serves it from a dedicated thread.
    ///
    /// The runner stays synchronous — it spends its life waiting on child
    /// processes — so the server gets one current-thread runtime of its own
    /// rather than colouring the whole crate `async`.
    pub(crate) fn start() -> Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let server_state = Arc::clone(&state);

        std::thread::Builder::new().name("edm-mock".to_owned()).spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        return;
                    }
                };
                let addr = match listener.local_addr() {
                    Ok(addr) => addr,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        return;
                    }
                };
                if tx.send(Ok(addr)).is_err() {
                    return;
                }
                loop {
                    let Ok((socket, _)) = listener.accept().await else { continue };
                    let state = Arc::clone(&server_state);
                    tokio::task::spawn(async move {
                        // A dropped connection is the client exiting, which is
                        // the normal end of every scenario.
                        let _ = serve(socket, state).await;
                    });
                }
            });
        })?;

        let addr = rx.recv().context("the mock server thread died before binding")??;
        Ok(Self { addr, state })
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Installs a scenario's script and clears the log.
    pub(crate) fn load(&self, scenario: &Scenario) {
        let mut state = self.lock();
        state.routes =
            scenario.routes.iter().map(|route| RouteState { route: route.clone(), served: 0 }).collect();
        state.log.clear();
        state.problems.clear();
        state.generation += 1;
    }

    /// The wire log, and anything that went wrong while producing it.
    pub(crate) fn take_log(&self, ordered: bool) -> (String, Vec<String>) {
        let state = self.lock();
        let mut problems = state.problems.clone();
        // A scripted reply nobody asked for is a scenario that has stopped
        // testing what it says it tests. Only worth reporting once the side
        // under test has made *some* request: a side that made none has a much
        // louder failure than this one.
        if !state.log.is_empty() {
            for entry in &state.routes {
                if entry.served < entry.route.replies.len() {
                    problems.push(format!(
                        "{} had {} of {} scripted replies left unused",
                        entry.route.path,
                        entry.route.replies.len() - entry.served,
                        entry.route.replies.len()
                    ));
                }
            }
        }
        (format_log(&state.log, ordered), problems)
    }

    pub(crate) fn request_count(&self) -> usize {
        self.lock().log.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic inside the server thread would leave the harness reporting a
        // green that means nothing, so it is not survivable.
        self.state.lock().expect("mock state poisoned")
    }
}

/// The wire log.
///
/// ```text
/// #<seq> <profile> <METHOD> <path>
/// ?<raw query>
/// <lowercased header>: <value>
/// .<body length>
/// <body, base64 if not UTF-8>
/// ```
///
/// `ordered` is false whenever more than one request can be in flight: arrival
/// order at a socket is a race between the two client implementations' internal
/// scheduling, not an observable of either program, so the records are sorted
/// into a canonical order and numbered afterwards.
fn format_log(records: &[Record], ordered: bool) -> String {
    let mut bodies: Vec<String> = records.iter().map(render_record).collect();
    if !ordered {
        bodies.sort();
    }
    let mut out = String::new();
    for (index, body) in bodies.iter().enumerate() {
        let _ = write!(out, "#{}", index + 1);
        out.push_str(body);
    }
    out
}

fn render_record(record: &Record) -> String {
    let mut out = format!(" {} {} {}\n?{}\n", record.profile, record.method, record.path, record.query);

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in &record.headers {
        match injected(record.profile, name) {
            Injected::Diffed => headers.push((name.clone(), value.clone())),
            // Presence, not value: the line still moves if the header stops
            // being sent.
            Injected::PresenceOnly => headers.push((name.clone(), "<injected>".to_owned())),
            Injected::Ignored => {}
        }
    }
    headers.sort();
    for (name, value) in headers {
        let _ = writeln!(out, "{name}: {value}");
    }

    let _ = writeln!(out, ".{}", record.body.len());
    match std::str::from_utf8(&record.body) {
        Ok(text) => out.push_str(text),
        Err(_) => out.push_str(&base64::engine::general_purpose::STANDARD.encode(&record.body)),
    }
    out.push('\n');
    out
}

async fn serve(mut socket: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    let mut buffer = Vec::new();
    loop {
        let Some(request) = read_request(&mut socket, &mut buffer).await? else { return Ok(()) };

        let profile = Profile::of_path(&request.path);
        let (reply, note) = choose(&state, &request, profile);
        if let Some(note) = note {
            state.lock().expect("mock state poisoned").problems.push(note);
        }
        if let Some(profile) = profile {
            let record = Record {
                profile,
                method: request.method.clone(),
                path: request.path.clone(),
                query: request.query.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
            };
            state.lock().expect("mock state poisoned").log.push(record);
        }

        let Some(reply) = reply else {
            write_reply(&mut socket, &not_found(), request.method == "HEAD").await?;
            continue;
        };
        if reply.never {
            park(&state).await;
            return Ok(());
        }
        if reply.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
        }
        write_reply(&mut socket, &reply, request.method == "HEAD").await?;
    }
}

/// Holds a connection open, answering nothing, until the scenario ends.
async fn park(state: &Arc<Mutex<State>>) {
    let generation = state.lock().expect("mock state poisoned").generation;
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if state.lock().expect("mock state poisoned").generation != generation {
            return;
        }
    }
}

fn choose(
    state: &Arc<Mutex<State>>,
    request: &Request,
    profile: Option<Profile>,
) -> (Option<Reply>, Option<String>) {
    let Some(profile) = profile else {
        return (None, Some(format!("no profile owns the path {}", request.path)));
    };
    // The market id lives inside the encrypted query, so a Companion API route
    // that wants to tell one poll from another has to be given the plaintext.
    let envelope = (profile == Profile::Frontier)
        .then(|| {
            let nonce = request.header("nonce")?;
            codec::open_envelope(&request.query, &nonce)
        })
        .flatten();
    let body = String::from_utf8_lossy(&request.body).into_owned();

    let mut state = state.lock().expect("mock state poisoned");
    let mut path_matched = false;
    for entry in &mut state.routes {
        if entry.route.path != request.path {
            continue;
        }
        if let Some(needle) = &entry.route.envelope
            && !envelope.as_deref().unwrap_or_default().contains(needle.as_str())
        {
            continue;
        }
        if let Some(needle) = &entry.route.body_contains
            && !body.contains(needle.as_str())
        {
            continue;
        }
        path_matched = true;
        if let Some(reply) = entry.route.replies.get(entry.served) {
            entry.served += 1;
            return (Some(reply.clone()), None);
        }
    }
    let note = if path_matched {
        format!("{} {} exhausted its scripted replies", request.method, request.path)
    } else {
        format!("nothing routes {} {}", request.method, request.path)
    };
    (None, Some(note))
}

/// The answer when nothing matched.
///
/// A 404 rather than a connection reset, and with a canonical phrase, because
/// both sides must fail the same way: an unrouted request should show up as a
/// mock problem plus identical output, never as a diff whose cause is the
/// harness.
fn not_found() -> Reply {
    Reply {
        status: 404,
        reason: "Not Found".to_owned(),
        headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
        body: b"edm-mock: no route".to_vec(),
        delay_ms: 0,
        never: false,
    }
}

async fn write_reply(socket: &mut TcpStream, reply: &Reply, head: bool) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {} {}\r\n", reply.status, reply.reason).into_bytes();
    for (name, value) in &reply.headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    if !reply.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-length")) {
        out.extend_from_slice(format!("Content-Length: {}\r\n", reply.body.len()).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    if !head {
        out.extend_from_slice(&reply.body);
    }
    socket.write_all(&out).await?;
    socket.flush().await
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<String> {
        // `Headers.get` joins duplicates with ", " **[R71]**; the mock reads
        // only `nonce`, which no client duplicates, but matching the semantics
        // costs one line.
        let joined: Vec<&str> = self
            .headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect();
        (!joined.is_empty()).then(|| joined.join(", "))
    }
}

/// Reads one request, keeping whatever the socket over-delivered for the next.
async fn read_request(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<Option<Request>> {
    let head_end = loop {
        if let Some(index) = find(buffer, b"\r\n\r\n") {
            break index;
        }
        let mut chunk = [0u8; 8192];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8(buffer[..head_end].to_vec()).context("non-UTF-8 request head")?;
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or_default();
    let mut parts = start.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (target.to_owned(), String::new()),
    };

    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        headers.push((name.trim_matches(' ').to_ascii_lowercase(), value.trim_matches(' ').to_owned()));
    }
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        bail!("edm-mock does not speak chunked requests; neither client should send one");
    }
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map_or(Ok(0), |(_, value)| value.parse())
        .context("bad Content-Length")?;

    let body_start = head_end + 4;
    while buffer.len() < body_start + length {
        let mut chunk = [0u8; 8192];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed mid-body");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = buffer[body_start..body_start + length].to_vec();
    buffer.drain(..body_start + length);

    Ok(Some(Request { method, path, query, headers, body }))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(profile: Profile, headers: &[(&str, &str)], body: &[u8]) -> Record {
        Record {
            profile,
            method: "GET".to_owned(),
            path: "/2.0/elite/market/list".to_owned(),
            query: "abc=".to_owned(),
            headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
            body: body.to_vec(),
        }
    }

    #[test]
    fn the_log_hides_injected_values_but_not_their_absence() {
        let text = render_record(&record(
            Profile::Frontier,
            &[("host", "127.0.0.1:9"), ("connection", "keep-alive"), ("nonce", "0123456789ab")],
            b"",
        ));
        assert!(text.contains("host: <injected>"), "{text}");
        assert!(!text.contains("connection"), "{text}");
        assert!(text.contains("nonce: 0123456789ab"), "{text}");
        assert!(text.ends_with(".0\n\n"), "{text}");
    }

    #[test]
    fn the_frontier_user_agent_is_diffed_and_the_ardent_one_is_not() {
        let frontier = render_record(&record(Profile::Frontier, &[("user-agent", "EDGame")], b""));
        assert!(frontier.contains("user-agent: EDGame"));
        let ardent = render_record(&record(Profile::Ardent, &[("user-agent", "edm/1.0.0")], b""));
        assert!(ardent.contains("user-agent: <injected>"));
    }

    #[test]
    fn a_non_utf8_body_is_recorded_as_base64() {
        let text = render_record(&record(Profile::Eddn, &[], &[0xff, 0xfe]));
        assert!(text.ends_with(".2\n//4=\n"), "{text}");
    }

    #[test]
    fn unordered_logs_are_canonicalised_before_numbering() {
        let one = record(Profile::Frontier, &[("nonce", "b")], b"");
        let two = record(Profile::Frontier, &[("nonce", "a")], b"");
        assert_eq!(
            format_log(&[one.clone(), two.clone()], false),
            format_log(&[two, one], false)
        );
    }

    #[test]
    fn serves_a_scripted_reply_and_records_the_request() {
        let mock = Mock::start().unwrap();
        {
            let mut state = mock.lock();
            state.routes = vec![RouteState {
                route: Route {
                    path: "/upload/".to_owned(),
                    envelope: None,
                    body_contains: None,
                    replies: vec![Reply {
                        status: 200,
                        reason: "OK".to_owned(),
                        headers: vec![],
                        body: b"OK".to_vec(),
                        delay_ms: 0,
                        never: false,
                    }],
                },
                served: 0,
            }];
        }
        // The client half needs a runtime of its own; the server already has
        // one on its own thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let text = runtime.block_on(async {
            let mut socket = TcpStream::connect(mock.addr).await.unwrap();
            socket
                .write_all(b"POST /upload/ HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabc")
                .await
                .unwrap();
            let mut response = vec![0u8; 256];
            let read = socket.read(&mut response).await.unwrap();
            String::from_utf8_lossy(&response[..read]).into_owned()
        });
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.ends_with("OK"), "{text}");

        let (log, problems) = mock.take_log(true);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(log.contains("#1 eddn POST /upload/"), "{log}");
        assert!(log.ends_with(".3\nabc\n"), "{log}");
    }
}
