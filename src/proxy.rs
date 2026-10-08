//! `zap-mcp serve`: a thin stdio↔HTTP relay to the daemon, for MCP clients
//! that can only launch commands (Claude Desktop). Starts the daemon if it
//! isn't running and reconnects if it restarts.

use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};
use tokio::sync::Mutex;

use crate::Paths;
use crate::daemon::{self, Settings};
use crate::server::INSTRUCTIONS;

type Upstream = RunningService<RoleClient, ()>;

struct Relay {
    paths: Arc<Paths>,
    settings: Settings,
    upstream: Mutex<Option<Upstream>>,
}

impl Relay {
    async fn connect(&self) -> Result<Upstream> {
        daemon::ensure_running(&self.paths, &self.settings).await?;
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.settings.url())
                .auth_header(self.settings.token.clone()),
        );
        ().serve(transport)
            .await
            .context("connecting to the zap-mcp daemon")
    }

    /// Run `op` against the daemon, reconnecting once if the connection broke
    /// (e.g. the daemon was restarted).
    async fn with_upstream<T, F, Fut>(&self, op: F) -> Result<T, McpError>
    where
        F: Fn(rmcp::Peer<RoleClient>) -> Fut,
        Fut: Future<Output = Result<T, rmcp::ServiceError>>,
    {
        let mut guard = self.upstream.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect().await.map_err(|e| {
                    McpError::internal_error(format!("{e:#}"), None)
                })?);
            }
            let peer = guard.as_ref().expect("connected above").peer().clone();
            match op(peer).await {
                Ok(v) => return Ok(v),
                // Tool/protocol errors come back as-is; only transport
                // failures warrant a reconnect.
                Err(rmcp::ServiceError::McpError(e)) => return Err(e),
                Err(e) if attempt == 0 => {
                    log::warn!("daemon connection lost ({e}); reconnecting");
                    *guard = None;
                }
                Err(e) => return Err(McpError::internal_error(e.to_string(), None)),
            }
        }
        unreachable!("loop returns on the second attempt")
    }
}

impl ServerHandler for Relay {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.with_upstream(|peer| {
            let request = request.clone();
            async move { peer.list_tools(request).await }
        })
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.with_upstream(|peer| {
            let request = request.clone();
            async move { peer.call_tool(request).await }
        })
        .await
        .map(Into::into)
    }
}

pub async fn run(paths: Paths, allow_send_flag: bool) -> Result<()> {
    let mut settings = Settings::load_or_create(&paths)?;
    if allow_send_flag && !settings.allow_send {
        // Registrations from before the daemon passed `serve --allow-send`;
        // the daemon now reads this from settings.json.
        settings.allow_send = true;
        settings.save(&paths)?;
    }
    let relay = Relay {
        paths: Arc::new(paths),
        settings,
        upstream: Mutex::new(None),
    };
    // Connect eagerly so a broken setup shows up in the client's log at
    // startup rather than on the first tool call.
    match relay.connect().await {
        Ok(up) => *relay.upstream.lock().await = Some(up),
        Err(e) => log::warn!("{e:#}"),
    }
    let service = relay
        .serve(rmcp::transport::stdio())
        .await
        .context("starting MCP relay")?;
    service.waiting().await.context("MCP relay")?;
    Ok(())
}
