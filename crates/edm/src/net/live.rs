//! The real transport: two `reqwest` clients, configured to differ.

use reqwest::redirect;

use super::{Body, HeaderView, HttpRequest, HttpResponse, HttpTransport, Profile, TransportError};

/// Bodies are capped well below the point where a hostile `Content-Length`
/// could exhaust memory. C4 — `vec![0; 9e15]` aborts the process, where
/// JavaScript would throw something catchable.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Two clients, because the game-internal API and the auxiliary services need
/// genuinely different behaviour.
#[derive(Debug)]
pub struct LiveHttp {
    game_api: reqwest::Client,
    aux: reqwest::Client,
}

impl LiveHttp {
    pub fn new() -> Result<Self, TransportError> {
        // No `.timeout()` and no `.connect_timeout()` anywhere. The original
        // sets neither: a sweep's per-attempt race is the only deadline in the
        // program, and a single `market --market-id` really can hang forever.
        // Adding one here would be an improvement that changes behaviour. R69.
        let game_api = reqwest::Client::builder()
            // The game-internal API answers a redirect with something worth seeing,
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

        Ok(Self { game_api, aux })
    }

    const fn client(&self, profile: Profile) -> &reqwest::Client {
        match profile {
            Profile::GameApi => &self.game_api,
            Profile::Aux => &self.aux,
        }
    }
}

impl HttpTransport for LiveHttp {
    async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| TransportError::Other(format!("bad method {}", request.method)))?;

        let mut builder = self.client(request.profile).request(method, request.url);
        // Set explicitly, because reqwest is no longer doing content coding for
        // us. This is the list Bun's fetch advertises.
        builder = builder.header("accept-encoding", "gzip, deflate, br");
        for (name, value) in request.headers {
            builder = builder.header(*name, value);
        }
        builder = match request.body {
            Body::None => builder,
            // `Content-Length: 0` and nothing else — no `content-type`,
            // measured against bun 1.2.3 with a raw socket server; R66's
            // Content-Type half is wrong. The length has to be set explicitly
            // because reqwest omits the header entirely for an empty body,
            // where `fetch` sends it, and the game-internal API's PUT routes want
            // the framing.
            Body::EmptyText => builder.header("content-length", "0").body(""),
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
        // The headers were captured *before* this, so `content-encoding` and
        // the compressed `content-length` are still in them — which is what
        // `fetch` shows and therefore what the RESPONSE table must print.
        let bytes = decode_content(&headers, &bytes)?;

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

/// Undoes `Content-Encoding`.
///
/// A body that arrived still compressed would fail the base64 gate and take
/// every request down with `Response is not valid standard Base64`, so this is
/// not optional — it is just done here instead of inside reqwest, so that the
/// headers survive to be printed.
fn decode_content(headers: &HeaderView, bytes: &[u8]) -> Result<Vec<u8>, TransportError> {
    use std::io::Read as _;

    let Some(encoding) = headers.get("content-encoding") else {
        return Ok(bytes.to_vec());
    };
    let mut out = Vec::new();
    let failed = |what: &str| TransportError::Other(format!("could not decode a {what} body"));

    match edm_core::js::text::js_trim(&encoding).to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map(|_| ())
            .map_err(|_| failed("gzip"))?,
        "deflate" => flate2::read::ZlibDecoder::new(bytes)
            .read_to_end(&mut out)
            .map(|_| ())
            .map_err(|_| failed("deflate"))?,
        "br" => brotli::Decompressor::new(bytes, 4096)
            .read_to_end(&mut out)
            .map(|_| ())
            .map_err(|_| failed("brotli"))?,
        // `identity`, or something no one advertised. Pass it through.
        _ => return Ok(bytes.to_vec()),
    }
    Ok(out)
}
