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
    Capi,
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
    /// Not the same as [`Body::None`]: `fetch` with `body: ""` adds
    /// `Content-Type: text/plain;charset=UTF-8` and `Content-Length: 0`, and
    /// the Companion API's PUT routes want that framing. Those headers appear
    /// on the wire but *not* in the request table the program prints. R66.
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

/// A drained set of response headers with WHATWG `Headers` semantics.
///
/// Three behaviours are load-bearing and none of them are what a `HashMap`
/// would give you. `get` joins duplicates with `", "` — which is how two
/// `uncompressedsize` headers become `"12, 34"`, then `NaN`, then a rejected
/// response. Iteration is lowercased and **sorted**, which fixes the row order
/// of every printed header table. And lookup is case-insensitive. R71.
#[derive(Clone, Debug, Default)]
pub struct HeaderView {
    /// Lowercased name, value — in insertion order; sorting happens on iteration.
    entries: Vec<(String, String)>,
}

impl HeaderView {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { entries: pairs.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect() }
    }

    /// `headers.get(name)` — every matching value, joined with `", "`, or
    /// `None` when the name is absent.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let wanted = name.to_lowercase();
        let mut found: Option<String> = None;
        for (key, value) in &self.entries {
            if *key != wanted {
                continue;
            }
            match &mut found {
                Some(joined) => {
                    joined.push_str(", ");
                    joined.push_str(value);
                }
                None => found = Some(value.clone()),
            }
        }
        found
    }

    /// `for (const [name, value] of headers)` — lowercased, sorted by name,
    /// duplicates already combined.
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
#[allow(async_fn_in_trait, reason = "single-threaded runtime; a Send bound would be noise")]
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

    /// Two headers of the same name are one value joined with `", "` — which is
    /// exactly how a duplicated `uncompressedsize` becomes unparseable and gets
    /// the response rejected. R71.
    #[test]
    fn duplicates_join_with_a_comma() {
        let headers = HeaderView::from_pairs([
            ("UncompressedSize".to_owned(), "12".to_owned()),
            ("uncompressedsize".to_owned(), "34".to_owned()),
        ]);
        assert_eq!(headers.get("uncompressedsize").as_deref(), Some("12, 34"));
        // Which is then `Number("12, 34")` — NaN — and the response is refused.
        assert!(edm_core::js::to_number("12, 34").is_nan());
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
            headers.sorted().iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
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
