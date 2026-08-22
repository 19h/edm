//! HTTP, behind a seam.
//!
//! The seam exists because the acceptance gate is a byte-diff against the
//! original run against the same mock server, and because the failure paths —
//! a 405 with an odd `Allow`, a 2xx that fails to decrypt, a body that arrives
//! gzipped — are most of what the transport code actually does and all of them
//! need to be reachable from a test.

pub mod live;

/// Which client a request goes through.
///
/// Not a stylistic split: the two behave differently on the wire. The Companion
/// API must not follow redirects, and the auxiliary services must — with
/// `fetch`'s limit of 20, not reqwest's default of 10. R67.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// `api.orerve.net`. Redirects are surfaced, not followed.
    GameApi,
    /// Ardent and EDDN.
    Aux,
}

/// What to send as a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body<'a> {
    /// No body at all — a GET or HEAD.
    None,
    /// The empty string.
    ///
    /// Not the same as [`Body::None`]: `fetch` with `body: ""` sends
    /// `Content-Length: 0`, which is the framing the game-internal API's PUT routes
    /// want, and that header appears on the wire but *not* in the request table
    /// the program prints.
    ///
    /// It does **not** send a `Content-Type`. R66 claims
    /// `text/plain;charset=UTF-8` and the register is wrong: measured against
    /// bun 1.2.3, a `""` body carries a length and nothing else.
    EmptyText,
    /// A JSON document, for EDDN.
    Json(&'a [u8]),
}

/// One outbound request.
#[derive(Clone, Debug)]
pub struct HttpRequest<'a> {
    pub profile: Profile,
    pub method: &'a str,
    pub url: &'a str,
    /// Sent in the order given. Names reach the wire lowercased either way.
    pub headers: &'a [(&'a str, String)],
    pub body: Body<'a>,
}

/// A response, already drained.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    pub status: u16,
    /// `StatusCode::canonical_reason()`, not the phrase from the wire —
    /// hyper's HTTP/1 client discards that. C3.
    pub status_text: String,
    pub headers: HeaderView,
    /// The body, decoded the way `Response.text()` decodes it. R62.
    pub body: String,
}

/// Why a request never produced a response.
///
/// The message text is a registered divergence: Bun and undici phrase transport
/// failures differently and unstably, so ours come from a table pinned by
/// running the original against each failure mode. C2.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("fetch failed")]
    Connect,
    #[error("fetch failed")]
    Tls,
    #[error("fetch failed")]
    Dns,
    #[error("The operation was aborted.")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

/// A drained set of response headers, as Bun's `fetch` presents them.
///
/// **R71 as written in the register is wrong, and this is what measurement
/// says instead.** The WHATWG `Headers` class does define `get` as a
/// comma-joined combination of every matching value, and a port written from
/// the specification will implement that — but Bun's HTTP client does not
/// *append* a repeated response header, it **overwrites**. So two
/// `uncompressedsize` headers of `512` and `4096` yield `4096`, not
/// `"512, 4096"`, and the response goes on to be decrypted at the wrong size
/// and refused by the LZ4 length check rather than by the header check. Two
/// `allow` headers on a 405 likewise yield only the last.
///
/// Verified against bun 1.2.3 with a raw socket server writing the duplicates
/// by hand; see `xtask/scenarios/fail-duplicate-size.toml`.
///
/// The other two behaviours are as the register describes: iteration is
/// lowercased and **sorted**, which fixes the row order of every printed header
/// table, and lookup is case-insensitive.
#[derive(Clone, Debug, Default)]
pub struct HeaderView {
    /// Lowercased name, value — in arrival order; the last of a name wins.
    entries: Vec<(String, String)>,
}

impl HeaderView {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            entries: pairs
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v))
                .collect(),
        }
    }

    /// `headers.get(name)` — the **last** value under that name, or `None`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let wanted = name.to_lowercase();
        self.entries
            .iter()
            .rev()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.clone())
    }

    /// `for (const [name, value] of headers)` — lowercased, sorted by name,
    /// one row per name.
    #[must_use]
    pub fn sorted(&self) -> Vec<(String, String)> {
        let mut names: Vec<&str> = self.entries.iter().map(|(k, _)| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .map(|name| (name.to_owned(), self.get(name).unwrap_or_default()))
            .collect()
    }
}

/// Somewhere a request can be sent.
///
/// `async fn` in the trait rather than an explicit `-> impl Future + Send`:
/// the runtime is single-threaded on purpose (deterministic output interleaving
/// is what makes the byte-diff gate possible), so nothing here ever crosses a
/// thread and a `Send` bound would only make the implementations harder to
/// write.
#[allow(
    async_fn_in_trait,
    reason = "single-threaded runtime; a Send bound would be noise"
)]
pub trait HttpTransport {
    async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError>;
}

/// `Response.text()`.
///
/// Unconditionally UTF-8 and lossy, ignoring any charset the `Content-Type`
/// declares, and stripping one leading byte-order mark. `reqwest`'s own
/// `text()` honours the declared charset, which would decode a mislabelled body
/// differently; `clippy.toml` bans it for that reason. R62.
#[must_use]
pub fn decode_body(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    match text.strip_prefix('\u{FEFF}') {
        Some(stripped) => stripped.to_owned(),
        None => text.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repeated response header overwrites; it does not combine.
    ///
    /// The register said otherwise, and the specification agrees with the
    /// register — but Bun's client does not append, so the last value is the
    /// only one anybody sees. Getting this wrong would print
    /// `Missing or invalid uncompressedSize header: 512, 4096` where the
    /// original decrypts at 4096 and fails later, in a different place, with a
    /// different message. R71, corrected by measurement.
    #[test]
    fn a_repeated_header_overwrites_rather_than_combining() {
        let headers = HeaderView::from_pairs([
            ("UncompressedSize".to_owned(), "512".to_owned()),
            ("uncompressedsize".to_owned(), "4096".to_owned()),
        ]);
        assert_eq!(headers.get("uncompressedsize").as_deref(), Some("4096"));
        assert_eq!(edm_core::js::to_number("4096"), 4096.0);

        // The same rule decides which verbs a 405 appears to allow.
        let headers = HeaderView::from_pairs([
            ("allow".to_owned(), "PUT".to_owned()),
            ("allow".to_owned(), "OPTIONS".to_owned()),
        ]);
        assert_eq!(headers.get("allow").as_deref(), Some("OPTIONS"));
        assert_eq!(
            headers.sorted(),
            vec![("allow".to_owned(), "OPTIONS".to_owned())]
        );
    }

    #[test]
    fn lookup_is_case_insensitive_and_iteration_is_sorted() {
        let headers = HeaderView::from_pairs([
            ("Nonce".to_owned(), "abc".to_owned()),
            ("Content-Type".to_owned(), "text/plain".to_owned()),
            ("allow".to_owned(), "PUT, OPTIONS".to_owned()),
        ]);
        assert_eq!(headers.get("NONCE").as_deref(), Some("abc"));
        assert_eq!(
            headers
                .sorted()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["allow", "content-type", "nonce"]
        );
    }

    #[test]
    fn missing_is_none_not_empty() {
        let headers = HeaderView::default();
        assert!(headers.get("nonce").is_none());
    }

    /// A byte-order mark is consumed, and invalid bytes become U+FFFD rather
    /// than failing the whole read.
    #[test]
    fn body_decoding_is_lossy_and_strips_one_bom() {
        assert_eq!(decode_body("\u{FEFF}hello".as_bytes()), "hello");
        assert_eq!(decode_body("\u{FEFF}\u{FEFF}x".as_bytes()), "\u{FEFF}x");
        assert_eq!(decode_body(&[0xff, 0xfe, b'a']), "\u{FFFD}\u{FFFD}a");
        assert_eq!(decode_body(b""), "");
    }
}
