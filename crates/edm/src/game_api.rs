//! The game-internal API session: credentials, per-request stamps, envelopes, and
//! the send/decode sequence.
//!
//! The sequence is not an implementation detail. `send` prints the request
//! table *before* it checks `--dry-run`, prints the response table from one of
//! two places depending on whether the caller asked for quiet, and validates a
//! 2xx in a fixed order — status, then the `Nonce` header, then
//! `uncompressedsize`, then the decrypt. Each of those steps has its own
//! message and its own exit-code effect, and the parity harness diffs all of
//! them. This module is written statement-for-statement against ts:1224-1290.

use edm_core::consts::{
    AUTH_TOKEN_MIN_LENGTH, DEFAULT_FDEV_SEASON, DEFAULT_FDEV_SEMVER, DEFAULT_USER_AGENT, Endpoint,
    MACHINE_TOKEN_LENGTH,
};
use edm_core::js::{self, text};
use edm_core::wire::{self, Nonce};

use crate::secret::Secret;

/// Long-lived session identity. The same values are reused by every endpoint.
///
/// `Debug` is derived rather than suppressed: the two tokens are [`Secret`]s,
/// whose own `Debug` prints a length. Deriving it here is a standing check that
/// the redaction survives being nested inside another struct.
#[derive(Debug)]
pub struct Credentials {
    pub commander_id: String,
    pub machine_id: String,
    pub machine_token: Secret,
    pub auth_token: Secret,
}

/// `validateAscii` (ts:59) — printable ASCII only, no control characters and
/// nothing above U+007E.
pub fn validate_ascii(name: &str, value: &str) -> Result<(), String> {
    // `/^[\x20-\x7e]+$/` — note the `+`, so the empty string fails too.
    let ok = !value.is_empty() && value.bytes().all(|b| (0x20..=0x7e).contains(&b));
    if ok { Ok(()) } else { Err(format!("{name} must contain printable ASCII only")) }
}

/// `validateExactLength` (ts:64), measured in UTF-16 code units.
pub fn validate_exact_length(name: &str, value: &str, expected: usize) -> Result<(), String> {
    let length = text::utf16_len(value);
    if length == expected {
        Ok(())
    } else {
        Err(format!("{name} must be exactly {expected} characters; received {length}"))
    }
}

/// A floor rather than an exact length \[C31\].
///
/// The original demands exactly 2024 characters. A live token measured
/// 2026-08-06 is 2022, so the constant is one observation written down as a
/// law — and it made the program refuse a credential the game itself was using.
/// A floor still catches the mistake the check exists for, a half-pasted token.
pub fn validate_min_length(name: &str, value: &str, minimum: usize) -> Result<(), String> {
    let length = text::utf16_len(value);
    if length >= minimum {
        Ok(())
    } else {
        Err(format!("{name} looks truncated: {length} characters, expected at least {minimum}"))
    }
}

impl Credentials {
    /// `loadCredentials` (ts:75).
    ///
    /// Field order and check order are both observable: each field is validated
    /// for ASCII *before* its length, and the four fields are read in source
    /// order, so which complaint a user sees depends on which of their
    /// credentials is wrong first. Every command loads these — including
    /// `markets --dry-run`, which never sends anything. R50.
    pub fn load(
        commander_id: &str,
        machine_id: &str,
        machine_token: &str,
        auth_token: &str,
    ) -> Result<Self, String> {
        validate_ascii("cmdrId", commander_id)?;
        validate_ascii("machineId", machine_id)?;
        validate_ascii("machineToken", machine_token)?;
        validate_exact_length("machineToken", machine_token, MACHINE_TOKEN_LENGTH)?;
        validate_ascii("authToken", auth_token)?;
        validate_min_length("authToken", auth_token, AUTH_TOKEN_MIN_LENGTH)?;

        Ok(Self {
            commander_id: commander_id.to_owned(),
            machine_id: machine_id.to_owned(),
            machine_token: Secret::new(machine_token.to_owned()),
            auth_token: Secret::new(auth_token.to_owned()),
        })
    }
}

/// The values the game regenerates for every single request.
#[derive(Clone, Debug)]
pub struct Stamp {
    pub nonce: Nonce,
    pub frontier_time: f64,
    /// A wrapping 32-bit millisecond uptime.
    pub request_time: u32,
}

/// One envelope field: the wire value plus how it should appear on screen.
#[derive(Clone, Debug)]
pub struct Field {
    pub name: &'static str,
    pub value: FieldValue,
}

#[derive(Clone)]
pub enum FieldValue {
    Text(String),
    Number(f64),
    /// A credential: goes on the wire in full, renders as a length.
    Masked { wire: String, shown: String },
}

impl std::fmt::Debug for FieldValue {
    /// Hand-written, because a derived one would print `wire` — the full
    /// 2024-character auth token — into any `{:?}` of a request, a panic
    /// message, or an error chain. This is the one place in the program where
    /// deriving `Debug` would be a credential leak.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(value) => f.debug_tuple("Text").field(value).finish(),
            Self::Number(value) => f.debug_tuple("Number").field(value).finish(),
            Self::Masked { shown, .. } => f.debug_tuple("Masked").field(shown).finish(),
        }
    }
}

impl FieldValue {
    fn wire(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            // Interpolated through `String(n)`, so `1e21` reaches the wire as
            // `1e+21` rather than as twenty-one zeros.
            Self::Number(n) => js::js_number(*n),
            Self::Masked { wire, .. } => wire.clone(),
        }
    }

    /// What the ENVELOPE band of the request table shows.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::Masked { shown, .. } => shown.clone(),
            other => other.wire(),
        }
    }
}

impl Field {
    #[must_use]
    pub fn text(name: &'static str, value: impl Into<String>) -> Self {
        Self { name, value: FieldValue::Text(value.into()) }
    }

    #[must_use]
    pub fn number(name: &'static str, value: f64) -> Self {
        Self { name, value: FieldValue::Number(value) }
    }

    #[must_use]
    pub fn secret(name: &'static str, value: &Secret) -> Self {
        Self {
            name,
            value: FieldValue::Masked { wire: value.expose().to_owned(), shown: value.masked() },
        }
    }
}

/// `credentialFields` (ts:209) — the tail every envelope shares.
#[must_use]
pub fn credential_fields(credentials: &Credentials, frontier_time: f64) -> Vec<Field> {
    vec![
        Field::number("fTime", frontier_time),
        Field::text("machineId", credentials.machine_id.clone()),
        Field::secret("machineToken", &credentials.machine_token),
        Field::secret("authToken", &credentials.auth_token),
    ]
}

/// `listEnvelopeFields` (ts:218).
#[must_use]
pub fn list_fields(market_id: &str, credentials: &Credentials, frontier_time: f64) -> Vec<Field> {
    let mut fields = vec![
        Field::text("marketId", market_id),
        Field::text("cmdrId", credentials.commander_id.clone()),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// The envelope observed for `/2.0/elite/vendors/items`.
///
/// `vendorType=1` is Pioneer Supplies.  Type 2 is the bartender's static
/// microresource exchange catalogue and has no premium suit or weapon stock.
#[must_use]
pub fn vendor_fields(
    market_id: &str,
    vendor_type: f64,
    credentials: &Credentials,
    frontier_time: f64,
) -> Vec<Field> {
    let mut fields = vec![
        Field::text("cmdrId", credentials.commander_id.clone()),
        Field::text("marketId", market_id),
        Field::number("vendorType", vendor_type),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// `starsystemEnvelopeFields` (ts:230).
#[must_use]
pub fn starsystem_fields(
    system_address: f64,
    language: &str,
    cached_timestamp: f64,
    credentials: &Credentials,
    frontier_time: f64,
) -> Vec<Field> {
    let mut fields = vec![
        Field::text("cmdrId", credentials.commander_id.clone()),
        // The one unvalidated field, so a non-ASCII value changes the
        // plaintext's byte length. R65.
        Field::text("language", language),
        Field::number("systemAddr", system_address),
        Field::number("cachedTimeStamp", cached_timestamp),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// Read-only finance-resource envelope, as emitted by the game.
#[must_use]
pub fn finance_fields(credentials: &Credentials, frontier_time: f64) -> Vec<Field> {
    let mut fields = vec![Field::text("cmdrId", credentials.commander_id.clone())];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// Read-only commodity-catalogue envelope. The observed client omits cmdrId.
#[must_use]
pub fn commodity_resource_fields(credentials: &Credentials, frontier_time: f64) -> Vec<Field> {
    credential_fields(credentials, frontier_time)
}

/// Read-only bulk market-data envelope.
///
/// Addresses are serialized as one exact comma-separated decimal string: no
/// `f64`, no repeated field and no spaces. Five is the official client's batch
/// policy even though the server has accepted larger probes.
pub fn marketdata_fields(
    system_addresses: &[u64],
    credentials: &Credentials,
    frontier_time: f64,
) -> Result<Vec<Field>, String> {
    if system_addresses.is_empty() {
        return Err("marketdata needs at least one system address".to_owned());
    }
    if system_addresses.len() > edm_core::consts::MARKETDATA_BATCH_MAX {
        return Err(format!(
            "marketdata accepts at most {} system addresses per client batch",
            edm_core::consts::MARKETDATA_BATCH_MAX
        ));
    }
    if system_addresses.contains(&0) {
        return Err("marketdata system addresses must be nonzero".to_owned());
    }
    let joined = system_addresses
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut fields = vec![
        Field::text("cmdrId", credentials.commander_id.clone()),
        Field::text("systemAddr", joined),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    Ok(fields)
}

/// One daily-digest page. The observed endpoint has no commander id.
#[must_use]
pub fn daily_digest_fields(
    language: &str,
    page_number: u32,
    credentials: &Credentials,
    frontier_time: f64,
) -> Vec<Field> {
    let mut fields = vec![
        Field::text("language", language),
        Field::number("pageNumber", f64::from(page_number)),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// `tradeEnvelopeFields` (ts:246).
///
/// The booleans go on the wire as `1`/`0`, and `finalQty` is the size the
/// stack ends up at rather than a copy of `qty` — sending `qty` there earns an
/// HTTP 402.
#[must_use]
pub fn trade_fields(
    plan: &edm_core::domain::trade::TradePlan,
    credentials: &Credentials,
    frontier_time: f64,
) -> Vec<Field> {
    let mut fields = vec![
        Field::text("cmdrId", credentials.commander_id.clone()),
        // Never parsed, so whatever the flag said reaches the wire. R53.
        Field::text("marketId", plan.market_id.clone()),
        Field::text("transactionType", plan.kind.as_str()),
        Field::number("commodityId", plan.commodity_id),
        Field::number("blackMarket", f64::from(u8::from(plan.black_market))),
        Field::number("stolen", f64::from(u8::from(plan.stolen))),
        Field::number("unitPrice", plan.unit_price),
        Field::number("qty", plan.qty),
        Field::number("finalQty", plan.final_qty),
    ];
    fields.extend(credential_fields(credentials, frontier_time));
    fields
}

/// `serializeEnvelope` (ts:204) — `k=v` joined with `&`, and nothing is
/// percent-encoded: the game concatenates these values directly.
#[must_use]
pub fn serialize_envelope(fields: &[Field]) -> String {
    let mut out = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(field.name);
        out.push('=');
        out.push_str(&field.value.wire());
    }
    out
}

/// A request that is ready to send, or to print under `--dry-run`.
#[derive(Debug)]
pub struct PreparedRequest {
    pub path: &'static str,
    pub method: String,
    pub url: String,
    /// In the order the original constructs them; the printed table sorts.
    pub headers: Vec<(&'static str, String)>,
    pub stamp: Stamp,
    pub fields: Vec<Field>,
    /// The plaintext's length in UTF-8 bytes.
    ///
    /// The plaintext itself is deliberately not retained: the original reads
    /// only `.length` from it (ts:1187, ts:1319), and it holds both tokens in
    /// the clear. Keeping the number and dropping the buffer costs nothing
    /// observable. C12.
    pub plaintext_bytes: usize,
}

/// How the request headers are configured, after flags and environment.
#[derive(Clone, Debug)]
pub struct HeaderConfig {
    pub fdev_semver: String,
    pub user_agent: String,
    pub fdev_season: String,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            fdev_semver: DEFAULT_FDEV_SEMVER.to_owned(),
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            fdev_season: DEFAULT_FDEV_SEASON.to_owned(),
        }
    }
}

/// `prepareRequest` (ts:1154).
///
/// Pure: given a stamp and an envelope it always produces the same URL, which
/// is what makes `--dry-run --full-url` with a pinned nonce a deterministic
/// fixture the harness can diff.
#[must_use]
pub fn prepare(
    origin: &str,
    endpoint: Endpoint,
    method_override: Option<&str>,
    fields: Vec<Field>,
    stamp: Stamp,
    headers: &HeaderConfig,
) -> PreparedRequest {
    let plaintext = serialize_envelope(&fields);
    let sealed = wire::seal_query(plaintext.as_bytes(), &stamp.nonce);
    // The origin is a parameter, not the constant, because the alternative —
    // building with the constant and rewriting the prefix afterwards — is a
    // step a caller can forget. One did: the sweep sent every market poll to
    // the live game-internal API while the harness believed it was talking to a
    // mock. A signature that cannot be satisfied without deciding the origin
    // makes that class of mistake unrepresentable.
    let url = format!("{origin}{}?{sealed}", endpoint.path);

    let request_headers = vec![
        ("Request-Time", js::js_number(f64::from(stamp.request_time))),
        // Constant on every attempt, including requeues — the original never
        // increments it. R70.
        ("Fdev-Retry", "0/2".to_owned()),
        ("Fdev-Semver", headers.fdev_semver.clone()),
        ("User-Agent", headers.user_agent.clone()),
        ("Fdev-Season", headers.fdev_season.clone()),
        ("Encrypted", "1".to_owned()),
        ("Nonce", stamp.nonce.as_str().to_owned()),
    ];

    PreparedRequest {
        path: endpoint.path,
        method: method_override.map_or_else(|| endpoint.method.to_owned(), str::to_owned),
        url,
        headers: request_headers,
        stamp,
        plaintext_bytes: plaintext.len(),
        fields,
    }
}

impl PreparedRequest {
    /// The encrypted query, without the `?`.
    #[must_use]
    pub fn query(&self) -> &str {
        self.url.split_once('?').map_or("", |(_, query)| query)
    }

    /// A body-bearing verb needs an explicit empty body so `Content-Length: 0`
    /// is sent. The decision is keyed on the *effective* method, so
    /// `--method POST` on a listing endpoint acquires a body. R66.
    #[must_use]
    pub fn body_kind(&self) -> crate::net::Body<'static> {
        if self.method == "GET" || self.method == "HEAD" {
            crate::net::Body::None
        } else {
            crate::net::Body::EmptyText
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials::load("F1234567", "machine-1", &"m".repeat(80), &"a".repeat(2024)).unwrap()
    }

    #[test]
    fn credential_validation_reports_ascii_before_length() {
        // A token that is both non-ASCII and the wrong length complains about
        // the ASCII first, because the checks nest that way. R50.
        let error = Credentials::load("F1", "m", "é", &"a".repeat(2024)).unwrap_err();
        assert_eq!(error, "machineToken must contain printable ASCII only");

        let error = Credentials::load("F1", "m", "short", &"a".repeat(2024)).unwrap_err();
        assert_eq!(error, "machineToken must be exactly 80 characters; received 5");
    }

    #[test]
    fn an_empty_credential_is_not_printable_ascii() {
        // The pattern is `+`, not `*`.
        assert!(validate_ascii("cmdrId", "").is_err());
    }

    /// The envelope is concatenated verbatim — no percent-encoding, and numbers
    /// go through `String(n)`.
    #[test]
    fn the_envelope_is_raw_concatenation() {
        let fields = list_fields("4306502403", &credentials(), 1_700_000_000.0);
        let plaintext = serialize_envelope(&fields);
        assert!(plaintext.starts_with("marketId=4306502403&cmdrId=F1234567&fTime=1700000000&"));
        assert!(plaintext.contains(&"m".repeat(80)), "the token reaches the wire in full");
    }

    /// The masked rendering is a length, and never the value — and neither is
    /// `Debug`, which is where a derived implementation would have leaked the
    /// whole auth token into any panic message that mentioned a request.
    #[test]
    fn the_printed_envelope_hides_the_tokens() {
        let request = prepare(
            edm_core::consts::API_ORIGIN,
            edm_core::consts::MARKET_LIST,
            None,
            list_fields("1", &credentials(), 0.0),
            Stamp {
                nonce: Nonce::parse_arg("0123456789ab").unwrap(),
                frontier_time: 0.0,
                request_time: 0,
            },
            &HeaderConfig::default(),
        );
        let debug = format!("{request:?}");
        assert!(!debug.contains(&"a".repeat(20)), "the auth token must not survive Debug");
        assert!(!debug.contains(&"m".repeat(20)), "the machine token must not survive Debug");
        assert!(debug.contains("2024 chars (hidden)"));
        assert!(!format!("{:?}", credentials()).contains(&"a".repeat(20)));

        let fields = list_fields("1", &credentials(), 0.0);
        let shown: Vec<String> = fields.iter().map(|f| f.value.display()).collect();
        assert!(shown.contains(&"80 chars (hidden)".to_owned()));
        assert!(shown.contains(&"2024 chars (hidden)".to_owned()));
        assert!(!shown.iter().any(|s| s.contains(&"m".repeat(10))));
    }

    /// GET carries no body; anything else carries an empty one, and the
    /// decision follows `--method`. R66.
    #[test]
    fn the_body_follows_the_effective_method() {
        let stamp = Stamp {
            nonce: Nonce::parse_arg("0123456789ab").unwrap(),
            frontier_time: 0.0,
            request_time: 0,
        };
        let request = prepare(
            edm_core::consts::API_ORIGIN,
            edm_core::consts::MARKET_LIST,
            None,
            list_fields("1", &credentials(), 0.0),
            stamp.clone(),
            &HeaderConfig::default(),
        );
        assert_eq!(request.method, "GET");
        assert!(matches!(request.body_kind(), crate::net::Body::None));

        let overridden = prepare(
            edm_core::consts::API_ORIGIN,
            edm_core::consts::MARKET_LIST,
            Some("PUT"),
            list_fields("1", &credentials(), 0.0),
            stamp,
            &HeaderConfig::default(),
        );
        assert!(matches!(overridden.body_kind(), crate::net::Body::EmptyText));
    }

    /// The sealed query is standard padded base64 appended raw, and it must
    /// survive URL parsing untouched — a corrupted query would be silent and
    /// total. R64.
    #[test]
    fn the_sealed_query_is_url_safe_as_written() {
        let stamp = Stamp {
            nonce: Nonce::parse_arg("0123456789ab").unwrap(),
            frontier_time: 1_700_000_000.0,
            request_time: 12345,
        };
        let request = prepare(
            edm_core::consts::API_ORIGIN,
            edm_core::consts::MARKET_LIST,
            None,
            list_fields("4306502403", &credentials(), 1_700_000_000.0),
            stamp,
            &HeaderConfig::default(),
        );
        let query = request.query();
        assert!(!query.is_empty());
        assert!(query.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
        assert!(request.url.starts_with("https://api.orerve.net/2.0/elite/market/list?"));
    }
    #[test]
    fn read_only_envelopes_keep_observed_order_and_exact_addresses() {
        let c = credentials();
        assert_eq!(
            serialize_envelope(&finance_fields(&c, 7.0))
                .split('&')
                .map(|f| f.split('=').next().unwrap())
                .collect::<Vec<_>>(),
            ["cmdrId", "fTime", "machineId", "machineToken", "authToken"]
        );
        assert_eq!(
            serialize_envelope(&commodity_resource_fields(&c, 7.0))
                .split('&')
                .map(|f| f.split('=').next().unwrap())
                .collect::<Vec<_>>(),
            ["fTime", "machineId", "machineToken", "authToken"]
        );
        let fields = marketdata_fields(&[11_665_265_337_753, 9_007_199_254_740_993], &c, 7.0)
            .expect("valid batch");
        let text = serialize_envelope(&fields);
        assert!(
            text.starts_with("cmdrId=F1234567&systemAddr=11665265337753,9007199254740993&fTime=7&"),
            "{text}"
        );
        assert!(
            !text.contains("9007199254740992"),
            "must not round through f64: {text}"
        );

        let digest = serialize_envelope(&daily_digest_fields("en", 38, &c, 7.0));
        assert!(
            digest.starts_with("language=en&pageNumber=38&fTime=7&"),
            "{digest}"
        );
    }

    #[test]
    fn vendor_envelope_matches_the_observed_get_request() {
        let fields = vendor_fields("4370953219", 1.0, &credentials(), 1_786_067_738.0);
        let keys = fields.iter().map(|field| field.name).collect::<Vec<_>>();
        assert_eq!(
            keys,
            [
                "cmdrId",
                "marketId",
                "vendorType",
                "fTime",
                "machineId",
                "machineToken",
                "authToken",
            ]
        );
        let request = prepare(
            edm_core::consts::API_ORIGIN,
            edm_core::consts::VENDOR_ITEMS,
            None,
            fields,
            Stamp {
                nonce: Nonce::parse_arg("0123456789ab").unwrap(),
                frontier_time: 1_786_067_738.0,
                request_time: 1,
            },
            &HeaderConfig::default(),
        );
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/2.0/elite/vendors/items");
    }

    #[test]
    fn marketdata_batch_policy_is_enforced_before_any_request() {
        let c = credentials();
        assert!(marketdata_fields(&[], &c, 0.0).is_err());
        assert!(marketdata_fields(&[0], &c, 0.0).is_err());
        assert!(marketdata_fields(&[1, 2, 3, 4, 5], &c, 0.0).is_ok());
        assert!(marketdata_fields(&[1, 2, 3, 4, 5, 6], &c, 0.0).is_err());
    }
}
