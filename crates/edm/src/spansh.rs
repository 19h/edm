//! Fetching from Spansh \[C36\].
//!
//! The request builder, the parser and every consistency guard are in
//! `edm_core::spansh`; this is only the part that needs a socket.
//!
//! Unlike `resolve_location`'s deliberately asymmetric error handling, nothing
//! here is swallowed. Ardent's module explains why an outage must not read as
//! an empty region; a Spansh outage is the same mistake pointed the other way —
//! "no carrier is restricted" is a *fuller* answer than the truth, and it
//! silently restores the very bug the filter exists to remove. So every failure
//! reaches the caller, and the caller refuses the run.

use edm_core::js::json::JsValue;
use edm_core::spansh;

use crate::net::{Body, HttpRequest, HttpResponse, HttpTransport, Profile};

pub struct SpanshClient<'a, H> {
    http: &'a H,
    base: &'a str,
}

impl<H> std::fmt::Debug for SpanshClient<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpanshClient")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl<'a, H: HttpTransport> SpanshClient<'a, H> {
    pub const fn new(http: &'a H, base: &'a str) -> Self {
        Self { http, base }
    }

    /// The market ids in `batch` whose published docking access is one of
    /// `access_values`.
    ///
    /// One POST, never paged: `size` is the batch length and a batch cannot
    /// match more rows than it has ids, so a second page could only ever be
    /// empty.
    pub async fn carriers_with_access(
        &self,
        batch: &[f64],
        access_values: &[&str],
    ) -> Result<Vec<f64>, String> {
        let url = spansh::search_url(self.base);
        let body = spansh::search_body(batch, access_values);
        let headers = [("accept", "application/json".to_owned())];

        let response: HttpResponse = self
            .http
            .send(HttpRequest {
                profile: Profile::Aux,
                method: "POST",
                url: &url,
                headers: &headers,
                body: Body::Json(&body),
            })
            .await
            .map_err(|e| format!("Spansh could not be reached: {e}"))?;

        if !(200..300).contains(&response.status) {
            return Err(format!(
                "Spansh replied HTTP {} {} for {url}",
                response.status, response.status_text
            ));
        }

        let document = JsValue::parse(&response.body)
            .map_err(|e| format!("Spansh sent a body that is not JSON: {e}"))?;

        spansh::parse_search(&document, batch, batch.len())
            .map_err(|refusal| format!("Spansh answered HTTP 200 but {refusal}"))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::net::{HeaderView, TransportError};

    /// Answers one scripted reply per call, and refuses an unscripted one so a
    /// test cannot pass by never reaching the wire at all.
    #[derive(Debug)]
    struct FakeSpansh {
        replies: RefCell<Vec<Result<(u16, String), String>>>,
        bodies: RefCell<Vec<String>>,
    }

    impl FakeSpansh {
        fn ok(body: &str) -> Self {
            Self::scripted(Ok((200, body.to_owned())))
        }

        fn scripted(reply: Result<(u16, String), String>) -> Self {
            Self {
                replies: RefCell::new(vec![reply]),
                bodies: RefCell::new(Vec::new()),
            }
        }
    }

    impl HttpTransport for FakeSpansh {
        async fn send(&self, request: HttpRequest<'_>) -> Result<HttpResponse, TransportError> {
            if let Body::Json(bytes) = request.body {
                self.bodies
                    .borrow_mut()
                    .push(String::from_utf8_lossy(bytes).into_owned());
            }
            let reply = self
                .replies
                .borrow_mut()
                .pop()
                .unwrap_or_else(|| Err("no scripted reply".to_owned()));
            match reply {
                Ok((status, body)) => Ok(HttpResponse {
                    status,
                    status_text: if status == 200 {
                        "OK".to_owned()
                    } else {
                        "Service Unavailable".to_owned()
                    },
                    headers: HeaderView::default(),
                    body,
                }),
                Err(message) => Err(TransportError::Other(message)),
            }
        }
    }

    fn ok_body(size: usize, ids: &[f64]) -> String {
        let rows: Vec<String> = ids
            .iter()
            .map(|id| format!(r#"{{"market_id":{id}}}"#))
            .collect();
        format!(r#"{{"size":{size},"results":[{}]}}"#, rows.join(","))
    }

    #[tokio::test]
    async fn a_clean_reply_returns_the_matching_ids_and_posts_the_batch() {
        let http = FakeSpansh::ok(&ok_body(2, &[3_711_014_400.0]));
        let client = SpanshClient::new(&http, "https://spansh.test/api");
        let found = client
            .carriers_with_access(&[3_711_014_400.0, 128_000_000.0], &spansh::RESTRICTED_ACCESS)
            .await
            .unwrap();
        assert_eq!(found, vec![3_711_014_400.0]);
        let sent = http.bodies.borrow();
        assert_eq!(sent.len(), 1, "one POST, never paged");
        assert!(sent[0].contains(r#""size":2"#), "{}", sent[0]);
        assert!(sent[0].contains("Squadron Friends"), "{}", sent[0]);
    }

    /// The whole failure policy in one assertion: a Spansh that is down must
    /// never resolve to "nothing is restricted".
    #[tokio::test]
    async fn a_non_2xx_is_an_error_not_an_empty_answer() {
        let http = FakeSpansh::scripted(Ok((503, "down".to_owned())));
        let client = SpanshClient::new(&http, "https://spansh.test/api");
        let error = client
            .carriers_with_access(&[1.0], &spansh::RESTRICTED_ACCESS)
            .await
            .unwrap_err();
        assert!(error.contains("HTTP 503"), "{error}");
    }

    #[tokio::test]
    async fn a_transport_failure_is_an_error_not_an_empty_answer() {
        let http = FakeSpansh::scripted(Err("connection reset".to_owned()));
        let client = SpanshClient::new(&http, "https://spansh.test/api");
        let error = client
            .carriers_with_access(&[1.0], &spansh::RESTRICTED_ACCESS)
            .await
            .unwrap_err();
        assert!(error.contains("could not be reached"), "{error}");
    }

    /// HTTP 200, a plausible body, and a page silently short of what was asked
    /// for. This is the reply that would quietly under-filter a whole region.
    #[tokio::test]
    async fn a_clamped_page_is_an_error() {
        let http = FakeSpansh::ok(&ok_body(25, &[1.0]));
        let client = SpanshClient::new(&http, "https://spansh.test/api");
        let error = client
            .carriers_with_access(&[1.0, 2.0], &spansh::RESTRICTED_ACCESS)
            .await
            .unwrap_err();
        assert!(error.contains("the page is short"), "{error}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_an_error() {
        let http = FakeSpansh::ok("<html>maintenance</html>");
        let client = SpanshClient::new(&http, "https://spansh.test/api");
        let error = client
            .carriers_with_access(&[1.0], &spansh::RESTRICTED_ACCESS)
            .await
            .unwrap_err();
        assert!(error.contains("not JSON"), "{error}");
    }
}
