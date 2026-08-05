//! The real transport: two `reqwest` clients, configured to differ.

use reqwest::redirect;

use super::{Body, HeaderView, HttpRequest, HttpResponse, HttpTransport, Profile, TransportError};

/// Bodies are capped well below the point where a hostile `Content-Length`
/// could exhaust memory. C4 — `vec![0; 9e15]` aborts the process, where
/// JavaScript would throw something catchable.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Two clients, because the Companion API and the auxiliary services need
/// genuinely different behaviour.
#[derive(Debug)]
pub struct LiveHttp {
    capi: reqwest::Client,
    aux: reqwest::Client,
}

impl LiveHttp {
    pub fn new() -> Result<Self, TransportError> {
        // No `.timeout()` and no `.connect_timeout()` anywhere. The original
        // sets neither: a sweep's per-attempt race is the only deadline in the
        // program, and a single `market --market-id` really can hang forever.
        // Adding one here would be an improvement that changes behaviour. R69.
        let capi = reqwest::Client::builder()
            // The Companion API answers a redirect with something worth seeing,
            // so it is surfaced rather than followed. R67.
            .redirect(redirect::Policy::none())
            .pool_max_idle_per_host(16)
            .tcp_nodelay(true)
            .build()
            .map_err(|e| TransportError::Other(e.to_string()))?;

        let aux = reqwest::Client::builder()
            // `fetch` follows up to 20; reqwest's default is 10. R67.
            .redirect(redirect::Policy::limited(20))
            // Bun would send `Bun/x.y` here. EDDN asks senders to identify
            // themselves, and Frontier requests carry their own per-request
            // agent, so this is only ever seen by the auxiliary services. C19.
            .user_agent(concat!("edm/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| TransportError::Other(e.to_string()))?;

        Ok(Self { capi, aux })
    }

    const fn client(&self, profile: Profile) -> &reqwest::Client {
        match profile {
            Profile::Capi => &self.capi,
            Profile::Aux => &self.aux,
        }
    }
}

impl HttpTransport for LiveHttp {
    async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| TransportError::Other(format!("bad method {}", request.method)))?;

        let mut builder = self.client(request.profile).request(method, request.url);
        for (name, value) in request.headers {
            builder = builder.header(*name, value);
        }
        // `Accept-Encoding` is never set by us: reqwest only decompresses
        // transparently when *it* chose the encoding, and a body that arrived
        // still gzipped would fail the base64 gate and take every request down
        // with "Response is not valid standard Base64".
        builder = match request.body {
            Body::None => builder,
            Body::EmptyText => {
                builder.header("content-type", "text/plain;charset=UTF-8").body("")
            }
            Body::Json(bytes) => {
                builder.header("content-type", "application/json").body(bytes.to_vec())
            }
        };

        let response = builder.send().await.map_err(|e| classify(&e))?;
        let status = response.status();
        let headers = HeaderView::from_pairs(response.headers().iter().map(|(name, value)| {
            // Header values are bytes; `fetch` exposes them isomorphic-decoded
            // rather than refusing them.
            let text = value.to_str().map_or_else(
                |_| value.as_bytes().iter().map(|b| char::from(*b)).collect(),
                str::to_owned,
            );
            (name.as_str().to_owned(), text)
        }));

        let bytes = response.bytes().await.map_err(|e| classify(&e))?;
        if bytes.len() > MAX_BODY_BYTES {
            return Err(TransportError::Other(format!(
                "response body exceeds {MAX_BODY_BYTES} bytes"
            )));
        }

        Ok(HttpResponse {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or("").to_owned(),
            headers,
            body: super::decode_body(&bytes),
        })
    }
}

/// Maps a transport failure onto the message the original would have printed.
///
/// Bun's are undici's, which are both unstable across versions and unreachable
/// from here; the harness pins ours against a recorded table instead. C2.
fn classify(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Aborted
    } else if error.is_connect() {
        TransportError::Connect
    } else {
        // Everything else — a malformed request, a body that stopped early, a
        // redirect limit — falls through with the underlying text, which the
        // harness allowlists rather than diffing.
        TransportError::Other(error.to_string())
    }
}
