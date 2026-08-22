//! One request/response round trip, in the original's exact order.
//!
//! Transcribed statement-for-statement from ts:1224-1290, because almost every
//! line of it is observable: which table prints and from where, which
//! diagnostic goes to stderr and which to stdout, and what the exit code ends up
//! as. The happy path is four lines; the rest is the failure taxonomy.

use edm_core::js;
use edm_core::wire::{self, Nonce};

use crate::game_api::PreparedRequest;
use crate::net::{HeaderView, HttpRequest, HttpResponse, HttpTransport, Profile};
use crate::out::{EXIT_FAILURE, Out};

/// What came back.
#[derive(Debug)]
pub struct Exchange {
    pub status: u16,
    pub status_text: String,
    pub headers: HeaderView,
    /// The decrypted body, or `None` when it could not be decoded.
    pub decrypted: Option<String>,
    /// The body as received, before decoding.
    pub raw: String,
}

/// How a caller wants this send handled.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendOptions {
    /// Suppress the request and response tables — used for the trade command's
    /// price lookup and for every poll inside a sweep.
    pub quiet: bool,
    /// Send even under `--dry-run`. Only ever set for read-only lookups: the
    /// trade price probe and the sweep's starsystem read. R74.
    pub ignore_dry_run: bool,
}

/// `send` (ts:1224).
///
/// The two closures are where the request and response tables go. They are
/// parameters rather than direct calls because this module has no business
/// knowing how a table is built, and because a test wants to run the whole
/// failure taxonomy with them stubbed out.
pub async fn send<H, FRequest, FResponse>(
    http: &H,
    out: &Out,
    request: &PreparedRequest,
    dry_run: bool,
    options: SendOptions,
    emit_request: FRequest,
    emit_response: FResponse,
) -> Option<Exchange>
where
    H: HttpTransport,
    FRequest: Fn(&PreparedRequest),
    FResponse: Fn(&Exchange),
{
    let json = out.is_json();

    // Printed *before* the dry-run bail, so `--dry-run` still shows what would
    // have been sent. R74.
    if !options.quiet && !json {
        emit_request(request);
    }
    if dry_run && !options.ignore_dry_run {
        return None;
    }

    let headers: Vec<(&str, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (*name, value.clone()))
        .collect();
    let response = http
        .send(HttpRequest {
            profile: Profile::GameApi,
            method: &request.method,
            url: &request.url,
            headers: &headers,
            body: request.body_kind(),
        })
        .await;

    let response: HttpResponse = match response {
        Ok(response) => response,
        Err(error) => {
            // `fetch` rejects, `main`'s catch prints the message alone. R82.
            out.set_exit(EXIT_FAILURE);
            out.error(&error.to_string());
            return None;
        }
    };

    let mut exchange = Exchange {
        status: response.status,
        status_text: response.status_text,
        headers: response.headers,
        decrypted: None,
        raw: response.body,
    };

    if !options.quiet && !json {
        emit_response(&exchange);
    }

    let ok = (200..300).contains(&exchange.status);
    if !ok {
        out.set_exit(EXIT_FAILURE);
        // The headers carry the diagnosis — `Allow`, the nonce — so they are
        // shown even for a poll that asked to stay quiet.
        if options.quiet && !json {
            emit_response(&exchange);
        }
        report_failure(out, request, &exchange);
        return Some(exchange);
    }

    // A 2xx is validated in a fixed order, and each step has its own message.
    let nonce_header = exchange.headers.get("nonce");
    let Some(nonce) = nonce_header
        .as_deref()
        .and_then(Nonce::from_response_header)
    else {
        out.error(&format!(
            "Missing or invalid response Nonce header: {}",
            // `JSON.stringify` — an absent header renders as an unquoted
            // `null`, a present one as a quoted string. R72.
            quote_or_null(nonce_header.as_deref())
        ));
        out.line(&exchange.raw);
        out.set_exit(EXIT_FAILURE);
        return Some(exchange);
    };

    let size_header = exchange.headers.get("uncompressedsize");
    let size = js::to_number(size_header.as_deref().unwrap_or(""));
    if !js::safe_int(size) || size <= 0.0 {
        out.error(&format!(
            "Missing or invalid uncompressedSize header: {}",
            // Interpolated through `String(v)`, so an absent header is the bare
            // word `null` here rather than a quoted one. R72.
            size_header.as_deref().unwrap_or("null")
        ));
        out.set_exit(EXIT_FAILURE);
        return Some(exchange);
    }

    match wire::open_response(&exchange.raw, &nonce, size as usize) {
        Ok(text) => exchange.decrypted = Some(text),
        Err(error) => {
            out.error(&format!("Could not decrypt response: {error}"));
            out.line(&exchange.raw);
            out.set_exit(EXIT_FAILURE);
        }
    }
    Some(exchange)
}

/// The non-2xx report: the failure line, the 405 diagnosis, and whatever of the
/// body can be made legible.
fn report_failure(out: &Out, request: &PreparedRequest, exchange: &Exchange) {
    let allowed = exchange.headers.get("allow");

    out.error(&format!(
        "{} {} failed: HTTP {} {}",
        request.method,
        request.path,
        js::js_number(f64::from(exchange.status)),
        exchange.status_text,
    ));

    // An empty `Allow` is falsy, so a 405 carrying one produces no diagnosis at
    // all rather than an empty sentence. R73.
    if exchange.status == 405
        && let Some(allowed) = allowed.as_deref().filter(|value| !value.is_empty())
    {
        let verbs: Vec<String> = allowed
            .split(',')
            .map(|verb| edm_core::js::text::js_trim(verb).to_uppercase())
            .filter(|verb| !verb.is_empty())
            .collect();

        if verbs.contains(&request.method) {
            out.error(&format!(
                "The server reports it accepts {allowed}, so the verb is not what it rejected"
            ));
        } else {
            // `Allow: ,` filters down to nothing, and `verbs[0]` is then
            // `undefined` — which interpolates literally. R73.
            out.error(&format!(
                "This endpoint accepts {allowed} — retry with --method {}",
                verbs.first().map_or("undefined", String::as_str)
            ));
        }
    }

    // Failure bodies are encrypted too, so decode what can be decoded rather
    // than dumping base64 at the terminal.
    let decoded = exchange.headers.get("nonce").and_then(|nonce| {
        wire::open_opaque(
            &exchange.raw,
            &nonce,
            exchange.headers.get("uncompressedsize").as_deref(),
        )
    });

    if let Some(decoded) = decoded {
        out.emit(&[edm_core::render::Block::Heading("ERROR PAYLOAD".to_owned())]);
        out.line(&decoded);
    } else if !edm_core::js::text::js_trim(&exchange.raw).is_empty() {
        out.line(&exchange.raw);
    }
}

/// `JSON.stringify(value)` for a header that may be absent.
fn quote_or_null(value: Option<&str>) -> String {
    value.map_or_else(
        || "null".to_owned(),
        |text| edm_core::js::json::JsValue::Str(text.into()).stringify_compact(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_header_renders_unquoted_and_a_present_one_quoted() {
        assert_eq!(quote_or_null(None), "null");
        assert_eq!(quote_or_null(Some("abc")), "\"abc\"");
        // The message is built from the raw header, so a value with a quote in
        // it is escaped rather than breaking the line.
        assert_eq!(quote_or_null(Some("a\"b")), "\"a\\\"b\"");
    }
}
