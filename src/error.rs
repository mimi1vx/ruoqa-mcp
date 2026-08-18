//! Classification of `ruoqa::Error` into caller-visible tool failures versus
//! protocol-level errors. See the README's "Errors" section for the `kind`
//! vocabulary this produces.

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

/// Longest openQA body preview kept in a tool-error payload. ruoqa's own
/// preview is 8 KiB, too large for a transcript; this re-caps it.
const BODY_PREVIEW_BYTES: usize = 512;

/// Split `e` into a caller-visible tool failure (`Ok(CallToolResult::error)`)
/// or a protocol-level error (`Err(ErrorData)`), per the classification
/// table: an executed tool that failed is the caller's business; a
/// misconfigured or non-routable server is ours.
pub(crate) fn classify(e: ruoqa::Error) -> Result<CallToolResult, ErrorData> {
    let message = e.to_string();
    match e {
        ruoqa::Error::Request { status, body, .. } => tool_error(
            status_kind(status.as_u16()),
            Some(status.as_u16()),
            message,
            Some(&body),
        ),
        ruoqa::Error::Connection { .. } => tool_error("connection", None, message, None),
        ruoqa::Error::DeadlineExceeded { .. } => tool_error("timeout", None, message, None),
        ruoqa::Error::BodyTooLarge { .. } => tool_error("response_too_large", None, message, None),
        ruoqa::Error::Parse(_) => tool_error("invalid_response", None, message, None),
        // Config, Tls, IncompleteCredentials, IncompatibleHttpClient,
        // InvalidRetryPolicy, InvalidPath, UnsupportedRequestUrl,
        // CrossOriginRequest, CrossOriginRedirect, OutsideBaseUrlPath,
        // TooManyRedirects, and whatever `#[non_exhaustive]` adds later: all
        // mean this deployment is wired wrong or refused to send the
        // request. No caller can act on them.
        _ => Err(ErrorData::internal_error(message, None)),
    }
}

/// Classify an HTTP status into the same `kind` vocabulary
/// [`classify`] uses for `ruoqa::Error::Request`. Shared with
/// `tools::artifact`, which maps status codes from raw (non-ruoqa) responses.
pub(crate) fn status_kind(status: u16) -> &'static str {
    match status {
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        429 => "rate_limited",
        400..=499 => "bad_request",
        _ => "server_error",
    }
}

/// Build a tool-level error result: `{"error": {"kind", "status"?, \
/// "message", "body"?}}`, `body` truncated to [`BODY_PREVIEW_BYTES`].
pub(crate) fn tool_error(
    kind: &str,
    status: Option<u16>,
    message: impl Into<String>,
    body: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    let mut error = json!({"kind": kind, "message": message.into()});
    if let Some(status) = status {
        error["status"] = json!(status);
    }
    if let Some(body) = body {
        error["body"] = json!(truncate(body, BODY_PREVIEW_BYTES));
    }
    Ok(CallToolResult::error(vec![ContentBlock::json(
        json!({ "error": error }),
    )?]))
}

/// Truncate `s` to at most `max_bytes` bytes, on a char boundary. Naive
/// `&s[..max_bytes]` panics if it lands inside a multi-byte UTF-8 sequence.
fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::{Method, StatusCode, Url};
    use rmcp::model::ContentBlock;
    use serde_json::Value;

    use super::*;

    fn request_error(status: u16, body: &str) -> ruoqa::Error {
        ruoqa::Error::Request {
            method: Method::GET,
            url: Url::parse("https://openqa.example/api/v1/jobs/1").unwrap(),
            status: StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
        }
    }

    fn error_payload(result: &CallToolResult) -> Value {
        assert_eq!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        serde_json::from_str(&text.text).expect("tool error payload is valid JSON")
    }

    #[test]
    fn request_401_is_unauthorized() {
        let result = classify(request_error(401, "auth required")).unwrap();
        let payload = error_payload(&result);
        assert_eq!(payload["error"]["kind"], "unauthorized");
        assert_eq!(payload["error"]["status"], 401);
        assert_eq!(payload["error"]["body"], "auth required");
    }

    #[test]
    fn request_403_is_forbidden() {
        let result = classify(request_error(403, "no")).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "forbidden");
    }

    #[test]
    fn request_404_is_not_found() {
        let result = classify(request_error(404, "Job 1 does not exist")).unwrap();
        let payload = error_payload(&result);
        assert_eq!(payload["error"]["kind"], "not_found");
        assert_eq!(payload["error"]["body"], "Job 1 does not exist");
    }

    #[test]
    fn request_429_is_rate_limited() {
        let result = classify(request_error(429, "slow down")).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "rate_limited");
    }

    #[test]
    fn request_other_4xx_is_bad_request() {
        let result = classify(request_error(418, "teapot")).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "bad_request");
    }

    #[test]
    fn request_5xx_is_server_error() {
        let result = classify(request_error(500, "boom")).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "server_error");
    }

    #[tokio::test]
    async fn connection_error_is_connection_kind() {
        // Port 0 is not a valid connect target, so this fails locally without
        // needing network access or a real listener.
        let source = reqwest::Client::new()
            .get("http://127.0.0.1:0")
            .send()
            .await
            .expect_err("connecting to port 0 must fail");
        let e = ruoqa::Error::Connection {
            url: Url::parse("http://127.0.0.1:0").unwrap(),
            source,
        };
        let result = classify(e).unwrap();
        let payload = error_payload(&result);
        assert_eq!(payload["error"]["kind"], "connection");
        assert!(payload["error"]["status"].is_null());
    }

    #[test]
    fn deadline_exceeded_is_timeout() {
        let e = ruoqa::Error::DeadlineExceeded {
            elapsed: Duration::from_secs(30),
        };
        let result = classify(e).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "timeout");
    }

    #[test]
    fn body_too_large_is_response_too_large() {
        let e = ruoqa::Error::BodyTooLarge {
            limit: 32 * 1024 * 1024,
        };
        let result = classify(e).unwrap();
        assert_eq!(
            error_payload(&result)["error"]["kind"],
            "response_too_large"
        );
    }

    #[test]
    fn parse_error_is_invalid_response() {
        let e = ruoqa::Error::Parse(Box::new(std::io::Error::other("bad json")));
        let result = classify(e).unwrap();
        assert_eq!(error_payload(&result)["error"]["kind"], "invalid_response");
    }

    #[test]
    fn config_error_stays_protocol_level() {
        let e = ruoqa::Error::Config(Box::new(std::io::Error::other("no client.conf")));
        let err = classify(e).unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn tls_error_stays_protocol_level() {
        let e = ruoqa::Error::Tls(Box::new(std::io::Error::other("bad cert")));
        assert!(classify(e).is_err());
    }

    #[test]
    fn multi_byte_body_over_limit_truncates_without_panicking() {
        // "é" is 2 bytes; 300 of them is 600 bytes, over BODY_PREVIEW_BYTES.
        let body: String = "é".repeat(300);
        let truncated = truncate(&body, BODY_PREVIEW_BYTES);
        assert!(truncated.len() <= BODY_PREVIEW_BYTES);
        // Must still be valid UTF-8 (a panic would already have failed this).
        assert!(!truncated.is_empty());
    }

    #[test]
    fn short_body_is_not_truncated() {
        assert_eq!(truncate("short", BODY_PREVIEW_BYTES), "short");
    }
}
