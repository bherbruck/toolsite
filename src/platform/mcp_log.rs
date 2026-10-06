//! One log line per MCP request: which method, which tool, which client,
//! who, and what came back. The transport answers 200 whether a client
//! liked the answer or not, so without this a connector that connects and
//! then falls silent leaves nothing to read.
//!
//! The body is buffered to read the JSON-RPC envelope and handed on intact.
//! Only the envelope is logged: never params, arguments, SQL or a token.

use crate::{accounts::users::User, platform::bearer::Caller};
use axum::{
    body::Body,
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// The same ceiling as an upload: `push_app` may carry a whole app inline.
const MAX_BODY: usize = crate::platform::upload::MAX_UPLOAD_BYTES;

/// What one request said about itself, from its envelope alone.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Envelope {
    pub method: String,
    pub tool: Option<String>,
    pub client: Option<String>,
    pub protocol: Option<String>,
}

pub(crate) fn summarize(body: &[u8]) -> Envelope {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Envelope {
            method: "<unparsed>".into(),
            ..Default::default()
        };
    };
    // A batch is summarised by its first message, marked as a batch.
    let (message, batch) = match &value {
        serde_json::Value::Array(items) => (items.first().cloned().unwrap_or_default(), true),
        other => (other.clone(), false),
    };
    let str_at = |path: &[&str]| -> Option<String> {
        let mut cursor = &message;
        for key in path {
            cursor = cursor.get(key)?;
        }
        cursor.as_str().map(str::to_string)
    };
    let mut method = str_at(&["method"]).unwrap_or_else(|| {
        if message.get("result").is_some() || message.get("error").is_some() {
            "<response>".into()
        } else {
            "<unparsed>".into()
        }
    });
    if batch {
        method.push_str(" (batch)");
    }
    Envelope {
        tool: str_at(&["params", "name"]),
        client: str_at(&["params", "clientInfo", "name"]),
        protocol: str_at(&["params", "protocolVersion"]),
        method,
    }
}

pub(crate) async fn log_mcp(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request too large").into_response(),
    };
    let envelope = summarize(&bytes);
    let user_agent = parts
        .headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    // Whoever the auth layer established: a publishing caller on /mcp, an
    // account on /me/mcp, or nobody for a static token.
    let email = parts
        .extensions
        .get::<Caller>()
        .and_then(|caller| caller.user.as_ref().map(|u| u.email.clone()))
        .or_else(|| parts.extensions.get::<User>().map(|u| u.email.clone()));
    let path = parts.uri.path().to_string();

    let response = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    tracing::info!(
        path = %path,
        method = %envelope.method,
        tool = %envelope.tool.as_deref().unwrap_or("-"),
        client = %envelope.client.as_deref().unwrap_or("-"),
        protocol = %envelope.protocol.as_deref().unwrap_or("-"),
        user_agent = %user_agent,
        email = %email.as_deref().unwrap_or("-"),
        status = %response.status().as_u16(),
        "mcp"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_names_the_method_the_tool_and_the_client_and_nothing_else() {
        let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"ChatGPT","version":"1"},"capabilities":{}}}"#;
        let e = summarize(init);
        assert_eq!(e.method, "initialize");
        assert_eq!(e.client.as_deref(), Some("ChatGPT"));
        assert_eq!(e.protocol.as_deref(), Some("2025-06-18"));

        let call = br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_sql","arguments":{"sql":"select secret from vault"}}}"#;
        let e = summarize(call);
        assert_eq!(e.method, "tools/call");
        assert_eq!(e.tool.as_deref(), Some("run_sql"));
        assert!(!format!("{e:?}").contains("vault"), "arguments leaked into the envelope");

        assert_eq!(summarize(b"not json").method, "<unparsed>");
        assert_eq!(summarize(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).method, "ping (batch)");
        assert_eq!(summarize(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).method, "<response>");
    }
}
