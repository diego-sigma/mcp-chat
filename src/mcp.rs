use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::stream::BoxStream;
use http::{HeaderMap, HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientJsonRpcMessage, Tool};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse};

use crate::auth::TokenManager;

#[derive(Debug, thiserror::Error)]
pub enum AuthedError {
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("token fetch: {0}")]
    TokenFetch(String),
}

/// A `StreamableHttpClient` that optionally injects a fresh bearer token
/// from a `TokenManager` on every request, transparently handling refresh.
/// When `tokens` is `None`, requests go out without an `Authorization`
/// header — useful for MCP servers that don't require auth.
#[derive(Clone)]
pub struct AuthedReqwest {
    inner: reqwest::Client,
    tokens: Option<TokenManager>,
}

impl AuthedReqwest {
    pub fn new(tokens: Option<TokenManager>, extra_headers: HeaderMap) -> Self {
        // Bake the extra headers into the reqwest client so every outgoing
        // request — POST, GET-stream, DELETE — carries them automatically.
        // Useful for harness experiments that want to plumb opaque state to
        // the MCP server without changing tool signatures.
        let inner = reqwest::Client::builder()
            .default_headers(extra_headers)
            .build()
            .expect("reqwest client build");
        Self { inner, tokens }
    }

    async fn token(&self) -> Result<Option<String>, StreamableHttpError<AuthedError>> {
        match &self.tokens {
            None => Ok(None),
            Some(tm) => tm
                .get_valid_token()
                .await
                .map(Some)
                .map_err(|e| StreamableHttpError::Client(AuthedError::TokenFetch(e.to_string()))),
        }
    }
}

fn remap(e: StreamableHttpError<reqwest::Error>) -> StreamableHttpError<AuthedError> {
    use StreamableHttpError as E;
    match e {
        E::Client(re) => E::Client(AuthedError::from(re)),
        E::Sse(x) => E::Sse(x),
        E::Io(x) => E::Io(x),
        E::UnexpectedEndOfStream => E::UnexpectedEndOfStream,
        E::UnexpectedServerResponse(x) => E::UnexpectedServerResponse(x),
        E::UnexpectedContentType(x) => E::UnexpectedContentType(x),
        E::ServerDoesNotSupportSse => E::ServerDoesNotSupportSse,
        E::ServerDoesNotSupportDeleteSession => E::ServerDoesNotSupportDeleteSession,
        E::TokioJoinError(x) => E::TokioJoinError(x),
        E::Deserialize(x) => E::Deserialize(x),
        E::TransportChannelClosed => E::TransportChannelClosed,
        E::AuthRequired(x) => E::AuthRequired(x),
        E::InsufficientScope(x) => E::InsufficientScope(x),
        E::ReservedHeaderConflict(x) => E::ReservedHeaderConflict(x),
        E::SessionExpired => E::SessionExpired,
        other => E::UnexpectedServerResponse(format!("{other}").into()),
    }
}

impl StreamableHttpClient for AuthedReqwest {
    type Error = AuthedError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self.token().await?;
        self.inner
            .post_message(uri, message, session_id, token, custom_headers)
            .await
            .map_err(remap)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let token = self.token().await?;
        self.inner
            .delete_session(uri, session_id, token, custom_headers)
            .await
            .map_err(remap)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let token = self.token().await?;
        self.inner
            .get_stream(uri, session_id, last_event_id, token, custom_headers)
            .await
            .map_err(remap)
    }
}

pub type McpService = RunningService<rmcp::RoleClient, ()>;

pub async fn connect(
    url: &str,
    tokens: Option<TokenManager>,
    extra_headers: HeaderMap,
) -> Result<McpService> {
    let client = AuthedReqwest::new(tokens, extra_headers);
    let config = StreamableHttpClientTransportConfig::with_uri(url);
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let service = ()
        .serve(transport)
        .await
        .context("MCP initialize handshake failed")?;
    Ok(service)
}

pub async fn list_tools(service: &McpService) -> Result<Vec<Tool>> {
    service
        .peer()
        .list_all_tools()
        .await
        .context("MCP tools/list failed")
}

pub async fn call_tool(
    service: &McpService,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<CallToolResult> {
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Some(args) = arguments {
        params = params.with_arguments(args);
    }
    service
        .peer()
        .call_tool(params)
        .await
        .context("MCP tools/call failed")
}
