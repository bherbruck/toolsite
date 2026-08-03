//! Just enough MCP client to call the server's tools.
//!
//! The transport is Streamable HTTP: `initialize` returns a session id in a
//! header, every later call must carry it, and responses are SSE-framed even
//! when a single JSON object is all that comes back.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};

pub struct Mcp {
    client: Client,
    url: String,
    token: String,
    session: String,
}

impl Mcp {
    pub fn connect(base_url: &str, token: &str) -> Result<Self> {
        let url = format!("{}/mcp", base_url.trim_end_matches('/'));
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        let response = client
            .post(&url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "toolsite-cli", "version": env!("CARGO_PKG_VERSION") },
                }
            }))
            .send()
            .with_context(|| format!("could not reach {url}"))?;

        if response.status() == 401 {
            bail!("{url} rejected the token (401). Check TOOLSITE_TOKEN.");
        }
        if !response.status().is_success() {
            bail!("{url} answered {}", response.status());
        }

        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("server did not issue a session id"))?
            .to_string();

        let mcp = Self {
            client,
            url,
            token: token.to_string(),
            session,
        };

        // The protocol expects this before any tool call.
        mcp.client
            .post(&mcp.url)
            .bearer_auth(&mcp.token)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &mcp.session)
            .json(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()?;

        Ok(mcp)
    }

    /// Calls a tool and returns its text content, or the error the tool
    /// reported.
    pub fn call(&self, tool: &str, arguments: Value) -> Result<String> {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            }))
            .send()?;

        let body = response.text()?;
        let payload = parse_sse(&body)
            .ok_or_else(|| anyhow!("could not parse a response from {tool}: {body}"))?;

        if let Some(error) = payload.get("error") {
            bail!("{tool} failed: {error}");
        }
        let result = payload
            .get("result")
            .ok_or_else(|| anyhow!("{tool} returned no result: {body}"))?;

        let text = result
            .get("content")
            .and_then(|content| content.get(0))
            .and_then(|first| first.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!("{text}");
        }
        Ok(text)
    }
}

/// Pulls the first JSON object out of an SSE stream.
fn parse_sse(body: &str) -> Option<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .find_map(|data| serde_json::from_str::<Value>(data).ok())
        // A server that answered with plain JSON is fine too.
        .or_else(|| serde_json::from_str::<Value>(body).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_sse_framed_reply_is_read() {
        let body = "data: \nid: 0\n\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n";
        assert_eq!(parse_sse(body).unwrap()["result"]["ok"], json!(true));
    }

    #[test]
    fn a_plain_json_reply_is_read() {
        let body = r#"{"jsonrpc":"2.0","result":{"ok":true}}"#;
        assert_eq!(parse_sse(body).unwrap()["result"]["ok"], json!(true));
    }
}
