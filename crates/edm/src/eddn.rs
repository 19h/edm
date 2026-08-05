//! Publishing a market to EDDN.
//!
//! One POST per market, and **never retried inside a run**: the specification
//! requires a minimum one-minute wait before retrying any failed message and
//! forbids retrying a 400 or a 426 at all, so a fast requeue would breach it.
//! That is why the sweep's requeue logic covers only the Frontier poll.

use edm_core::js::text::{self, Metric};

use crate::net::{Body, HttpRequest, HttpTransport, Profile};

/// What became of one submission.
#[derive(Clone, Debug, PartialEq)]
pub struct EddnResult {
    pub ok: bool,
    pub status: Option<u16>,
    /// `"OK"`, or the status and the first 120 characters of the reply.
    pub detail: String,
    pub commodities: usize,
}

/// `submitToEddn` (ts:2952).
pub async fn submit<H: HttpTransport>(
    http: &H,
    url: &str,
    payload: &[u8],
    count: usize,
) -> EddnResult {
    let response = http
        .send(HttpRequest {
            profile: Profile::Aux,
            method: "POST",
            url,
            headers: &[],
            body: Body::Json(payload),
        })
        .await;

    match response {
        Ok(response) => {
            let body = text::js_trim(&response.body);
            // Success is `200` **and** a body of exactly `OK`. A 202, or a 200
            // carrying anything else, is a failure — the gateway uses the body
            // to report a schema rejection. R79.
            let ok = response.status == 200 && body == "OK";
            EddnResult {
                ok,
                status: Some(response.status),
                detail: if ok {
                    "OK".to_owned()
                } else {
                    format!("{} {}", response.status, text::clamp(body, 120, Metric::Utf16))
                },
                commodities: count,
            }
        }
        // A transport failure is reported and dropped, like every other EDDN
        // outcome. Nothing here is retried.
        Err(error) => EddnResult {
            ok: false,
            status: None,
            detail: error.to_string(),
            commodities: count,
        },
    }
}
