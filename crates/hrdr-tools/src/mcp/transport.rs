use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::oneshot;

use hrdr_llm::SseDecoder;

use crate::truncate;

use super::types::Pending;
use super::{HttpTransport, PROTOCOL_VERSION, SseTransport, StdioTransport};

/// Shared plumbing for stdio + SSE: register an id in `pending`, execute
/// `send_fn` (which should fire off the request), then race `rx` against
/// `timeout`. On send failure or timeout the id is cleaned up.
pub(crate) async fn request_via_pending<F, Fut>(
    pending: &Pending,
    id: u64,
    timeout: Duration,
    send_fn: F,
) -> Result<Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (tx, rx) = oneshot::channel();
    {
        let mut p = pending.lock().await;
        p.insert(id, tx);
    }
    if let Err(e) = send_fn().await {
        pending.lock().await.remove(&id);
        return Err(anyhow!("send failed: {e}"));
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(msg)) => Ok(msg),
        Ok(Err(_)) => bail!("connection closed"),
        Err(_) => {
            pending.lock().await.remove(&id);
            bail!("request timed out")
        }
    }
}

/// stdio: register the id, write the line, await the raw response message.
pub(crate) async fn stdio_request(
    t: &StdioTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let msg = req.to_string();
    request_via_pending(&t.pending, id, timeout, || async move {
        t.stdin_tx
            .send(msg)
            .map_err(|_| anyhow!("server is not running"))
    })
    .await
}

/// Streamable HTTP: POST the request; parse the JSON or SSE response for `id`.
pub(crate) async fn http_request(
    t: &HttpTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let resp = tokio::time::timeout(timeout, http_post(t, &req).send())
        .await
        .map_err(|_| anyhow!("timed out"))?
        .context("request failed")?;
    // Capture the session id (returned on `initialize`) for later requests.
    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *t.session.lock().unwrap() = Some(sid.to_string());
    }
    let status = resp.status();
    let is_sse = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("text/event-stream"));
    let body = resp.text().await.context("reading response")?;
    if !status.is_success() {
        bail!("HTTP {status}: {}", truncate(body.trim(), 500));
    }
    if is_sse {
        parse_sse_for_id(&body, id)
    } else {
        serde_json::from_str(&body).with_context(|| format!("decoding response: {body}"))
    }
}

/// Fire-and-forget HTTP POST (for notifications).
pub(crate) async fn http_send(t: &HttpTransport, msg: &Value) -> Result<()> {
    http_post(t, msg).send().await.context("request failed")?;
    Ok(())
}

/// Legacy HTTP+SSE: POST the request to the endpoint; the response arrives back
/// on the persistent SSE stream and is delivered via `pending`.
pub(crate) async fn sse_request(
    t: &SseTransport,
    id: u64,
    req: Value,
    timeout: Duration,
) -> Result<Value> {
    let post_url = t
        .post_url
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("no endpoint"))?;
    request_via_pending(&t.pending, id, timeout, || async {
        let resp = t
            .http
            .post(&post_url)
            .headers(t.headers.clone())
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::Error::new(e).context("request failed"))?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        Ok(())
    })
    .await
}

/// Build a [`HeaderMap`] from `(name, value)` pairs (config auth headers).
pub(crate) fn build_headers(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid MCP header name '{k}'"))?;
        let val = HeaderValue::from_str(v)
            .with_context(|| format!("invalid MCP header value for '{k}'"))?;
        map.insert(name, val);
    }
    Ok(map)
}

/// Build a POST request with the MCP headers + session id.
pub(crate) fn http_post(t: &HttpTransport, body: &Value) -> reqwest::RequestBuilder {
    let mut req = t
        .http
        .post(&t.url)
        .headers(t.headers.clone())
        .header(ACCEPT, "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PROTOCOL_VERSION)
        .json(body);
    if let Some(sid) = t.session.lock().unwrap().clone() {
        req = req.header("Mcp-Session-Id", sid);
    }
    req
}

/// Find the JSON-RPC message with `id` in an SSE stream body.
///
/// Uses [`SseDecoder`] for correct blank-line-terminated event grouping and
/// multi-line `data:` folding (mirrors the Streamable-HTTP inline SSE path).
/// A trailing `\n\n` is pushed after the body to flush any event that was not
/// terminated in the buffer (some servers omit the final blank line).
pub(crate) fn parse_sse_for_id(body: &str, id: u64) -> Result<Value> {
    let mut dec = SseDecoder::new();
    dec.push(body.as_bytes());
    // Force-flush a trailing event that isn't blank-line-terminated.
    dec.push(b"\n\n");
    for ev in dec.drain() {
        if let Ok(v) = serde_json::from_str::<Value>(&ev.data)
            && v.get("id").and_then(Value::as_u64) == Some(id)
        {
            return Ok(v);
        }
    }
    bail!("no response for request {id} in the SSE stream")
}
