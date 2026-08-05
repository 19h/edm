//! The command layer: `main`'s dispatch (ts:3134) and the three commands
//! behind it.
//!
//! Everything below this module is pure or nearly so, and everything here is
//! sequencing. That is the whole job: which table prints before which network
//! call, which diagnostic escapes the `--json` guard, and which failure leaves
//! the run going with an exit code of 1 rather than ending it.
//!
//! Three inherited behaviours shape the shape of this module.
//!
//! **Nothing exits early.** A command reports a failure by returning its
//! message; the caller prints it and *assigns* the exit code, exactly as
//! `process.exitCode = 1` does \[R75\]. There is no `std::process::exit`
//! anywhere and `clippy.toml` denies it.
//!
//! **`--json` is not a global mode.** The TypeScript guards it at about twenty
//! individual sites and misses several, so a failure diagnostic lands in the
//! middle of the JSON stream and corrupts it \[R76\]. That is reproduced. The
//! sites this layer owns route through [`App::leak`], which is also where
//! `EDM_STRICT_JSON=1` diverts them to stderr.
//!
//! **The read order of the command line is observable.** All of it lives in
//! [`edm_core::cli::config`], and nothing here re-derives a value that module
//! already computes \[R50\].

pub mod market;
pub mod route;
pub mod markets;
pub mod trade;

use std::borrow::Cow;
use std::time::Duration;

use edm_core::cli::config::{self, SessionConfig, StampDefaults};
use edm_core::cli::{self, Args, Cli, CliError, EnvSnapshot, Flag};
use edm_core::consts::{
    API_ORIGIN, ARDENT_BASE_URL, DEFAULT_FDEV_SEASON, DEFAULT_FDEV_SEMVER, DEFAULT_USER_AGENT,
    EDDN_UPLOAD_URL, Endpoint, MARKET_LIST,
};
use edm_core::js::json::{JsObject, JsValue};
use edm_core::js::{self, text::Metric};
use edm_core::render::{Block, views, write_blocks};
use edm_core::wire::Nonce;

use crate::capi::{self, Field, FieldValue, HeaderConfig, PreparedRequest, Stamp};
use crate::exchange::{self, Exchange, SendOptions};
use crate::net::{HeaderView, HttpTransport};
use crate::out::{EXIT_FAILURE, EXIT_USAGE, Out};
use crate::ports::{Clock, Entropy, Fs, Ports};
use crate::secret::Secret;

/// A command's failure: the message `main` prints alone, with no cause chain
/// \[R82\].
pub type CmdError = String;

/// What a command returns. `Ok` covers a run that set exit 1 along the way —
/// only a *thrown* error reaches the caller as `Err` \[R75\].
pub type CmdResult = Result<(), CmdError>;

// ---------------------------------------------------------------------------
// Ambient overrides
// ---------------------------------------------------------------------------

/// The three endpoint overrides the harness needs \[C24\], plus the two opt-in
/// fixes this layer can honour.
///
/// Unset, every field holds the constant the TypeScript compiles in, so a
/// default run is byte-identical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overrides {
    /// `EDM_ORIGIN_OVERRIDE` — replaces `API_ORIGIN` in both the sent URL and
    /// the printed `endpoint` row.
    pub origin: String,
    /// `EDM_ARDENT_BASE` — replaces `ARDENT_MODULE`, which Rust cannot import
    /// \[C1\].
    pub ardent_base: String,
    /// `EDM_EDDN_URL`.
    pub eddn_url: String,
    /// `EDM_STRICT_JSON=1` — routes R76's stray diagnostics to stderr so a
    /// `--json` run is parseable.
    pub strict_json: bool,
    /// `EDM_WIDTH=display` — the `unicode-width` cell metric.
    pub metric: Metric,
}

impl Overrides {
    #[must_use]
    pub fn from_env(env: &EnvSnapshot) -> Self {
        Self {
            origin: env.get("EDM_ORIGIN_OVERRIDE").unwrap_or(API_ORIGIN).to_owned(),
            ardent_base: env.get("EDM_ARDENT_BASE").unwrap_or(ARDENT_BASE_URL).to_owned(),
            eddn_url: env.get("EDM_EDDN_URL").unwrap_or(EDDN_UPLOAD_URL).to_owned(),
            strict_json: env.get("EDM_STRICT_JSON") == Some("1"),
            metric: if env.get("EDM_WIDTH") == Some("display") {
                Metric::Display
            } else {
                Metric::Utf16
            },
        }
    }
}

impl Default for Overrides {
    fn default() -> Self {
        Self::from_env(&EnvSnapshot::empty())
    }
}

/// A `setTimeout` delay, as Node schedules it.
///
/// Node clamps a delay outside `1..=INT32_MAX` to **one millisecond** and emits
/// a `TimeoutOverflowWarning`; the clamp is preserved and the warning is not
/// \[C22\]. Reproducing the clamp matters because `--timeout 1e10` gives every
/// sweep attempt a one-millisecond deadline rather than an eleven-day one.
#[must_use]
pub fn timer_duration(millis: f64) -> Duration {
    // NaN fails `contains`, which lands it on the same one-millisecond floor
    // Node gives it.
    let clamped = if (1.0..=2_147_483_647.0).contains(&millis) { millis } else { 1.0 };
    Duration::from_millis(clamped as u64)
}

/// The three stamp fields a flag can pin.
///
/// The sweep pool draws its own stamps inside the worker, so it needs the
/// overrides as data rather than as an accessor it could call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StampOverrides {
    pub nonce: Option<Nonce>,
    pub frontier_time: Option<f64>,
    pub request_time: Option<u32>,
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

/// Everything a command needs that it cannot compute for itself.
///
/// `Debug` is hand-written: none of the four ports need to be printable for a
/// panic message to be useful, and deriving it would demand bounds the real
/// implementations have no reason to carry.
pub struct App<'a, H, C, E, F> {
    pub http: &'a H,
    pub ports: &'a Ports<C, E, F>,
    pub out: &'a Out,
    pub cli: Cli<'a>,
    pub session: SessionConfig,
    /// The same four secrets as `session.credentials`, wrapped so that neither
    /// token can reach a `Debug` or an error chain.
    pub credentials: capi::Credentials,
    pub headers: HeaderConfig,
    pub overrides: &'a Overrides,
}

impl<H, C, E, F> std::fmt::Debug for App<'_, H, C, E, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("session", &self.session)
            .field("overrides", &self.overrides)
            .finish_non_exhaustive()
    }
}

impl<'a, H: HttpTransport, C: Clock, E: Entropy, F: Fs> App<'a, H, C, E, F> {
    /// `openSession` (ts:1133) plus the header block `prepareRequest` reads.
    ///
    /// The three header flags are read once here rather than per request. The
    /// TypeScript reads them inside `prepareRequest` (ts:1166), but none of
    /// them can fail and none of them can change mid-run, so the only thing
    /// that read order could affect is unobservable.
    pub fn open(
        cli: Cli<'a>,
        http: &'a H,
        ports: &'a Ports<C, E, F>,
        out: &'a Out,
        overrides: &'a Overrides,
    ) -> Result<Self, CmdError> {
        let session = config::open_session(&cli).map_err(message)?;
        let credentials = capi::Credentials {
            commander_id: session.credentials.commander_id.clone(),
            machine_id: session.credentials.machine_id.clone(),
            machine_token: Secret::new(session.credentials.machine_token.clone()),
            auth_token: Secret::new(session.credentials.auth_token.clone()),
        };
        let headers = HeaderConfig {
            fdev_semver: cli
                .optional_value(Flag::FdevSemver, Some("FDEV_SEMVER"))
                .unwrap_or(DEFAULT_FDEV_SEMVER)
                .to_owned(),
            user_agent: cli
                .optional_value(Flag::UserAgent, Some("USER_AGENT"))
                .unwrap_or(DEFAULT_USER_AGENT)
                .to_owned(),
            fdev_season: cli
                .optional_value(Flag::FdevSeason, Some("FDEV_SEASON"))
                .unwrap_or(DEFAULT_FDEV_SEASON)
                .to_owned(),
        };
        Ok(Self { http, ports, out, cli, session, credentials, headers, overrides })
    }

    /// `nextStamp` (ts:100), drawn afresh for every request \[R50\].
    pub fn stamp(&self) -> Result<Stamp, CmdError> {
        let stamp = config::next_stamp(
            &self.cli,
            StampDefaults {
                entropy: self.ports.entropy.nonce_bytes(),
                now_ms: self.ports.clock.now_ms(),
                uptime_seconds: self.ports.clock.uptime_seconds(),
            },
        )
        .map_err(message)?;
        Ok(Stamp {
            nonce: stamp.nonce,
            frontier_time: stamp.frontier_time,
            request_time: stamp.request_time,
        })
    }

    /// The pinned halves of the stamp, for the sweep pool — which draws its own
    /// stamps inside the worker and so cannot call [`App::stamp`].
    ///
    /// Validating them here rather than per request is not observable: the
    /// sweep's starsystem read has already drawn a stamp through
    /// [`App::stamp`], so a malformed `--nonce` has already thrown.
    pub fn stamp_overrides(&self) -> Result<StampOverrides, CmdError> {
        let nonce = match self.cli.optional_value(Flag::Nonce, Some("NONCE")) {
            Some(raw) => Some(Nonce::parse_arg(raw).map_err(|error| error.to_string())?),
            None => None,
        };
        let frontier_time = match self.cli.optional_value(Flag::FTime, Some("F_TIME")) {
            Some(raw) => Some(js::parse_unsigned_integer("fTime", raw)?),
            None => None,
        };
        let request_time = match self.cli.optional_value(Flag::RequestTime, Some("REQUEST_TIME")) {
            Some(raw) => Some(js::to_uint32(js::parse_unsigned_integer("requestTime", raw)?)),
            None => None,
        };
        Ok(StampOverrides { nonce, frontier_time, request_time })
    }

    /// `prepareRequest` (ts:1154), retargeted at `EDM_ORIGIN_OVERRIDE`.
    ///
    /// The override is applied by rewriting the origin the sealed URL already
    /// carries rather than by threading it through the sealing, because the
    /// query is encrypted from the envelope alone and the origin is not part of
    /// it \[C24\].
    pub fn prepare(&self, endpoint: Endpoint, fields: Vec<Field>, stamp: Stamp) -> PreparedRequest {
        capi::prepare(&self.overrides.origin, endpoint, self.session.method_override.as_deref(), fields, stamp, &self.headers)
    }

    /// `send` (ts:1224), with the two tables wired in.
    ///
    /// `send` itself calls neither closure when `quiet` or `--json` is set, so
    /// they carry no guard of their own.
    pub async fn send(&self, request: &PreparedRequest, options: SendOptions) -> Option<Exchange> {
        exchange::send(
            self.http,
            self.out,
            request,
            self.session.dry_run,
            options,
            |request| emit_request(self.out, request, &self.overrides.origin, self.session.full_url),
            |exchange| emit_response(self.out, exchange),
        )
        .await
    }

    /// `fetchMarket` (ts:1332).
    pub async fn fetch_market(
        &self,
        market_id: &str,
        options: SendOptions,
    ) -> Result<Option<Exchange>, CmdError> {
        let stamp = self.stamp()?;
        let request = self.prepare(
            MARKET_LIST,
            capi::list_fields(market_id, &self.credentials, stamp.frontier_time),
            stamp,
        );
        Ok(self.send(&request, options).await)
    }

    /// `requireMarketSnapshot` (ts:1931).
    ///
    /// Returns the whole parsed document rather than a [`MarketSnapshot`],
    /// because the snapshot borrows from it and the caller has to own the
    /// backing value.
    ///
    /// Both `ignoreDryRun` sites in the program are read-only lookups, and this
    /// is one of them: the listing is fetched even under `--dry-run` so that the
    /// plan can be resolved against real prices \[R74\].
    ///
    /// [`MarketSnapshot`]: edm_core::domain::MarketSnapshot
    pub async fn require_market_snapshot(&self, market_id: &str) -> Result<JsValue, CmdError> {
        let lookup =
            self.fetch_market(market_id, SendOptions { quiet: true, ignore_dry_run: true }).await?;
        let Some(text) = decrypted(lookup.as_ref()) else {
            // ts:1935
            return Err(
                "Could not read the market listing; retry with --no-resolve and explicit values"
                    .to_owned(),
            );
        };
        if let Some(document) = JsValue::parse(text).ok().filter(is_market_listing) {
            return Ok(document);
        }
        // Unguarded by `--json` in the original, so it corrupts the stream
        // \[R76\].
        self.leak(&views::opaque_payload(text));
        // ts:1939
        Err("Market listing did not contain commodity data".to_owned())
    }

    /// `emitNote` (ts:473).
    pub fn note(&self, text: String) {
        self.out.emit(&[Block::Note(text)]);
    }

    /// One of R76's leaks: output the TypeScript sends to **stdout** even under
    /// `--json`, corrupting the document it is in the middle of printing.
    ///
    /// Reproduced by default. `EDM_STRICT_JSON=1` sends it to stderr instead,
    /// which is the only way to pipe a `--json` run into a parser and have it
    /// survive a market that answers with an error envelope.
    pub fn leak(&self, blocks: &[Block<'_>]) {
        if !(self.session.json && self.overrides.strict_json) {
            self.out.emit(blocks);
            return;
        }
        let mut text = String::new();
        write_blocks(&mut text, blocks, self.out.width(), self.out.metric());
        // `error` supplies the final newline that `write_blocks` already added.
        self.out.error(text.strip_suffix('\n').unwrap_or(&text));
    }

    /// `emitJson` (ts:1302).
    ///
    /// `extra` is spread between `request` and `status`, so its keys land in
    /// their own order in the middle of the document \[R6\].
    pub fn emit_json(
        &self,
        request: &PreparedRequest,
        exchange: Option<&Exchange>,
        extra: Vec<(&str, JsValue)>,
    ) {
        let payload = exchange.and_then(|exchange| exchange.decrypted.as_deref());
        // `JSON.parse` of a body that is not JSON leaves the *string* in place,
        // which is why this is not simply `Null` on failure.
        let parsed = payload.map_or(JsValue::Null, |text| {
            JsValue::parse(text).unwrap_or_else(|_| JsValue::Str(text.into()))
        });

        let headers = object(
            request_headers(request)
                .into_iter()
                .map(|(name, value)| (name, JsValue::Str(value.into_boxed_str()))),
        );
        let envelope = object(
            request
                .fields
                .iter()
                .map(|field| (field.name.to_owned(), field_json(&field.value))),
        );

        let mut entries = vec![(
            "request".to_owned(),
            object([
                ("method".to_owned(), JsValue::Str(request.method.clone().into_boxed_str())),
                (
                    "endpoint".to_owned(),
                    JsValue::Str(
                        format!("{}{}", self.overrides.origin, request.path).into_boxed_str(),
                    ),
                ),
                ("url".to_owned(), JsValue::Str(request.url.clone().into_boxed_str())),
                ("headers".to_owned(), headers),
                ("envelope".to_owned(), envelope),
                ("plaintextLength".to_owned(), JsValue::Num(request.plaintext_bytes as f64)),
                // `...request.stamp`, in the stamp's own field order (ts:95).
                ("nonce".to_owned(), JsValue::Str(request.stamp.nonce.as_str().into())),
                ("frontierTime".to_owned(), JsValue::Num(request.stamp.frontier_time)),
                ("requestTime".to_owned(), JsValue::Num(f64::from(request.stamp.request_time))),
            ]),
        )];
        entries.extend(extra.into_iter().map(|(key, value)| (key.to_owned(), value)));
        entries.push((
            "status".to_owned(),
            exchange.map_or(JsValue::Null, |exchange| JsValue::Num(f64::from(exchange.status))),
        ));
        entries.push(("payload".to_owned(), parsed));

        self.out.line(&object(entries).stringify(2));
    }
}

// ---------------------------------------------------------------------------
// Shared rendering
// ---------------------------------------------------------------------------

/// `emitRequest` (ts:1175).
fn emit_request(out: &Out, request: &PreparedRequest, origin: &str, full_url: bool) {
    let headers = request_headers(request);
    let fields: Vec<(&str, Cow<'_, str>)> = request
        .fields
        .iter()
        .map(|field| (field.name, Cow::Owned(field.value.display())))
        .collect();
    let view = views::RequestView {
        method: &request.method,
        path: request.path,
        origin,
        url: &request.url,
        headers: &headers,
        fields: &fields,
        plaintext_bytes: request.plaintext_bytes as f64,
        nonce: request.stamp.nonce.as_str(),
        frontier_time: request.stamp.frontier_time,
        request_time: f64::from(request.stamp.request_time),
    };
    out.emit(&views::request(&view, full_url));
}

/// `emitResponse` (ts:1200).
/// The RESPONSE table.
///
/// `pub(crate)` because the sweep needs it too: a quiet poll still prints this
/// table when the status is not 2xx, since the headers carry the diagnosis
/// \[R74\].
pub(crate) fn emit_response(out: &Out, exchange: &Exchange) {
    let headers = exchange.headers.sorted();
    out.emit(&views::response(f64::from(exchange.status), &exchange.status_text, &headers));
}

/// The request headers as a `Headers` object presents them: lowercased, sorted,
/// duplicates combined \[R71\].
fn request_headers(request: &PreparedRequest) -> Vec<views::Header> {
    HeaderView::from_pairs(
        request.headers.iter().map(|(name, value)| ((*name).to_owned(), value.clone())),
    )
    .sorted()
}

/// `field.display ?? field.value` — which keeps a *number* a number, so
/// `fTime` and `systemAddr` are JSON numbers and the two tokens are their
/// masked strings.
fn field_json(value: &FieldValue) -> JsValue {
    match value {
        FieldValue::Number(number) => JsValue::Num(*number),
        other => JsValue::Str(other.display().into_boxed_str()),
    }
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// Builds an object with its keys in the order given, which is what a
/// JavaScript object literal enumerates in for non-index keys \[R5\].
pub(crate) fn object<I: IntoIterator<Item = (String, JsValue)>>(entries: I) -> JsValue {
    JsValue::Obj(JsObject::from_document_order(
        entries.into_iter().map(|(key, value)| (key.into_boxed_str(), value)).collect(),
    ))
}

pub(crate) fn str_value(text: &str) -> JsValue {
    JsValue::Str(text.into())
}

pub(crate) fn num_or_null(value: Option<f64>) -> JsValue {
    value.map_or(JsValue::Null, JsValue::Num)
}

/// `exchange?.decrypted` under JavaScript truthiness: an empty body is falsy,
/// so it is not a payload.
pub(crate) fn decrypted(exchange: Option<&Exchange>) -> Option<&str> {
    exchange
        .and_then(|exchange| exchange.decrypted.as_deref())
        .filter(|text| !text.is_empty())
}

fn is_market_listing(document: &JsValue) -> bool {
    edm_core::domain::parse_market_snapshot(document).is_some()
}

/// An accessor error becomes an ordinary thrown error: the message alone, exit
/// 1 \[R82\].
#[expect(
    clippy::needless_pass_by_value,
    reason = "every caller is a `map_err`, which hands the error over by value"
)]
pub(crate) fn message(error: CliError) -> CmdError {
    error.message().to_owned()
}

/// `fieldRow` (ts:506) for the two-column `FIELD_COLUMNS` tables this layer
/// builds itself.
pub(crate) fn field(name: &'static str, value: String) -> edm_core::render::Row<'static> {
    edm_core::render::Row::data([name.to_owned(), value])
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Would this command line have set `session.json`?
///
/// `main` needs the answer before it can build [`Out`], and `openSession` —
/// which is where the TypeScript reads it — cannot run until the credentials
/// have loaded. Peeking at the slot is exact for every case that matters: a
/// poisoned `--json` throws out of `openSession` before anything is sent, and
/// text can never land in a switch's slot \[C18\].
#[must_use]
pub fn wants_json(parsed: &cli::Parsed) -> bool {
    let args = parsed.route.as_ref().or(parsed.base.as_ref().ok());
    matches!(args, Some(args) if matches!(args.get(Flag::Json), Some(cli::Value::Bool(true))))
}

/// `main` (ts:3134), minus the process-level parts that belong to the binary.
///
/// The order of the first three tests is load-bearing: the `help` **command**
/// is checked before the `--help` **switch**, and both before the known-command
/// set, which is why `edm bogus --help` prints the help text and exits 0
/// \[R48\].
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    parsed: cli::Parsed,
    env: &EnvSnapshot,
    http: &H,
    ports: &Ports<C, E, F>,
    out: &Out,
    overrides: &Overrides,
) {
    // The extended parse, when there is one, is the only place a route-only
    // flag resolves \[C26\]. It is consulted *before* the base parse's error,
    // because `edm route Sol --radius 50` is a base parse failure and a valid
    // route command, and the base failure is the wrong answer to print.
    if let Some(route) = parsed.route {
        route_command(&route, env, http, ports, out, overrides).await;
        return;
    }

    let args = match parsed.base {
        Ok(args) => args,
        Err(error) => {
            // ts:3139 — the message and a blank line on stderr, `USAGE` on
            // stdout \[R49\].
            out.error_paragraph(&error.to_string());
            out.line(&cli::usage());
            out.set_exit(EXIT_USAGE);
            return;
        }
    };

    let cli = Cli::new(&args, env);
    if args.command == "help" {
        out.line(&cli::usage());
        return;
    }
    match cli.switch_value(Flag::Help, false) {
        Ok(true) => {
            out.line(&cli::usage());
            return;
        }
        Ok(false) => {}
        Err(error) => {
            // The `--help` read sits outside `main`'s try/catch, so Bun reports
            // it as an unhandled rejection: same message, same exit code, and a
            // stack trace we do not reproduce \[C17\].
            out.error(error.message());
            out.set_exit(EXIT_FAILURE);
            return;
        }
    }

    if !cli::is_known_command(&args.command) {
        // ts:3153
        out.error_paragraph(&format!("Unknown command \"{}\"", args.command));
        out.line(&cli::usage());
        out.set_exit(EXIT_USAGE);
        return;
    }

    // ts:3158 — the one try/catch, whose handler prints `error.message` alone
    // and assigns exit 1 \[R82\], \[R75\].
    if let Err(error) = dispatch(&args, cli, http, ports, out, overrides).await {
        out.error(&error);
        out.set_exit(EXIT_FAILURE);
    }
}

/// `edm route`, which the TypeScript does not have \[C25\].
///
/// Its own entry point rather than a fifth arm of `dispatch`, because none of
/// the ported preamble applies to it: it has its own help text (the pinned one
/// gains not a character), its own configuration reader, and no `openSession`
/// until it is about to spend a request. Keeping it out of `run`'s body is
/// what makes "route cannot change what `market` does" checkable by reading.
async fn route_command<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    args: &Args,
    env: &EnvSnapshot,
    http: &H,
    ports: &Ports<C, E, F>,
    out: &Out,
    overrides: &Overrides,
) {
    let cli = Cli::new(args, env);
    if cli.switch_value(Flag::Help, false).unwrap_or(false) {
        out.line(&cli::route_usage());
        return;
    }

    let config = match config::route_config(&cli) {
        Ok(config) => config,
        Err(error) => {
            out.error_paragraph(error.message());
            out.set_exit(EXIT_USAGE);
            return;
        }
    };

    // `openSession` last, and only because the sweep will need it. A route run
    // that is going to be refused by the ceiling should be refused whether or
    // not the machine has credentials — the plan is arithmetic, not access.
    let app = match App::open(cli, http, ports, out, overrides) {
        Ok(app) => app,
        Err(error) => {
            out.error(&error);
            out.set_exit(EXIT_FAILURE);
            return;
        }
    };
    // `RealTimer` here rather than a fourth port: `route::run` is generic over
    // the timer, so a test can still pin the delay sequence, and no ported
    // command grows a type parameter for a seam only this one uses.
    if let Err(error) = route::run(&app, &config, &crate::ports::RealTimer).await {
        out.error(&error);
        out.set_exit(EXIT_FAILURE);
    }
}

async fn dispatch<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    args: &Args,
    cli: Cli<'_>,
    http: &H,
    ports: &Ports<C, E, F>,
    out: &Out,
    overrides: &Overrides,
) -> CmdResult {
    let app = App::open(cli, http, ports, out, overrides)?;
    match args.command.as_str() {
        "trade" => trade::run(&app).await,
        "markets" => markets::run(&app).await,
        // `market` and `list` are the same command; anything else was rejected
        // by the known-command test above.
        _ => market::run(&app).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Node's `setTimeout` clamps anything outside `1..=INT32_MAX` to a single
    /// millisecond, so an absurd `--timeout` makes every attempt fail instantly
    /// rather than never \[C22\].
    #[test]
    fn a_timer_delay_outside_int32_clamps_to_one_millisecond() {
        assert_eq!(timer_duration(10_000.0), Duration::from_secs(10));
        assert_eq!(timer_duration(2_147_483_647.0), Duration::from_millis(2_147_483_647));
        assert_eq!(timer_duration(2_147_483_648.0), Duration::from_millis(1));
        assert_eq!(timer_duration(0.0), Duration::from_millis(1));
        assert_eq!(timer_duration(f64::NAN), Duration::from_millis(1));
    }

    /// Unset, every override is the constant the TypeScript compiles in \[C24\].
    #[test]
    fn an_empty_environment_leaves_every_endpoint_where_it_was() {
        let overrides = Overrides::default();
        assert_eq!(overrides.origin, API_ORIGIN);
        assert_eq!(overrides.ardent_base, ARDENT_BASE_URL);
        assert_eq!(overrides.eddn_url, EDDN_UPLOAD_URL);
        assert!(!overrides.strict_json);
        assert_eq!(overrides.metric, Metric::Utf16);
    }

    #[test]
    fn the_json_peek_agrees_with_the_switch_accessor() {
        for (argv, expected) in [
            (vec!["market".to_owned()], false),
            (vec!["market".to_owned(), "--json".to_owned()], true),
            (vec!["market".to_owned(), "--json".to_owned(), "false".to_owned()], false),
            (vec!["market".to_owned(), "--no-json".to_owned()], false),
        ] {
            let parsed = cli::parse_dispatch(&argv);
            let args = parsed.base.as_ref().expect("parses");
            let env = EnvSnapshot::empty();
            let accessor = Cli::new(args, &env).switch_value(Flag::Json, false).expect("readable");
            assert_eq!(wants_json(&parsed), expected);
            assert_eq!(wants_json(&parsed), accessor);
        }
    }

    /// `Out` is built from the peek before either parse is used, so `route
    /// --json` must reach it through the extended arm — otherwise the route
    /// command would print tables into a stream a caller is parsing.
    #[test]
    fn the_json_peek_sees_the_extended_parse_too() {
        let argv = vec!["route".to_owned(), "Sol".to_owned(), "--json".to_owned()];
        let parsed = cli::parse_dispatch(&argv);
        assert!(parsed.route.is_some(), "route must take the extended arm");
        assert!(wants_json(&parsed));
    }
}
