use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientInfo;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::CreateElicitationResult;
use rmcp::model::LoggingLevel;
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::rmcp_client::SendElicitation;

// Global queue for MCP resource notifications to inject into model context.
// Used by on_resource_updated to queue messages, drained by codex core after tool calls.
static PENDING_INJECTIONS: std::sync::LazyLock<std::sync::Mutex<Vec<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Drain all pending injection messages (called from codex core after tool outputs).
pub fn take_pending_injections() -> Vec<String> {
    std::mem::take(
        &mut *PENDING_INJECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

#[derive(Clone)]
pub(crate) struct LoggingClientHandler {
    client_info: ClientInfo,
    send_elicitation: Arc<SendElicitation>,
}

impl LoggingClientHandler {
    pub(crate) fn new(client_info: ClientInfo, send_elicitation: SendElicitation) -> Self {
        Self {
            client_info,
            send_elicitation: Arc::new(send_elicitation),
        }
    }
}

impl ClientHandler for LoggingClientHandler {
    async fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        (self.send_elicitation)(context.id, request)
            .await
            .map(Into::into)
            .map_err(|err| rmcp::ErrorData::internal_error(err.to_string(), None))
    }

    async fn on_cancelled(
        &self,
        params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!(
            "MCP server cancelled request (request_id: {}, reason: {:?})",
            params.request_id, params.reason
        );
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!(
            "MCP server progress notification (token: {:?}, progress: {}, total: {:?}, message: {:?})",
            params.progress_token, params.progress, params.total, params.message
        );
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        context: NotificationContext<RoleClient>,
    ) {
        let uri = params.uri.to_string();
        let server_name = context
            .peer
            .peer_info()
            .map(|info| info.server_info.name.as_str())
            .unwrap_or("unknown");

        info!(
            "MCP server resource updated (server: {}, uri: {})",
            server_name, uri
        );

        // Read the resource and format injection based on content type
        let injection = match context
            .peer
            .read_resource(rmcp::model::ReadResourceRequestParams {
                meta: None,
                uri: uri.clone(),
            })
            .await
        {
            Ok(result) => {
                let mut parts = Vec::new();
                for item in result.contents {
                    match item {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                            parts.push(format!(
                                "<resource server=\"{server_name}\" uri=\"{uri}\">\n{text}\n</resource>",
                            ));
                        }
                        rmcp::model::ResourceContents::BlobResourceContents {
                            blob,
                            mime_type,
                            ..
                        } => {
                            let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                            parts.push(format!(
                                "<resource server=\"{server_name}\" uri=\"{uri}\" type=\"blob\" mime-type=\"{mime}\" size=\"{}\">\n[Binary resource - use ReadMcpResourceTool to retrieve if needed]\n</resource>",
                                blob.len()
                            ));
                        }
                    }
                }
                format!(
                    "<resource-updated server=\"{server_name}\" uri=\"{uri}\" />\n{}",
                    parts.join("\n")
                )
            }
            Err(e) => {
                warn!("Failed to read resource {}: {:?}", uri, e);
                format!(
                    "<resource-updated server=\"{server_name}\" uri=\"{uri}\" />\n<resource server=\"{server_name}\" uri=\"{uri}\">\n[error reading resource: {e:?}]\n</resource>",
                )
            }
        };

        PENDING_INJECTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(injection);
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server resource list changed");
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server tool list changed");
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server prompt list changed");
    }

    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let LoggingMessageNotificationParam {
            level,
            logger,
            data,
        } = params;
        let logger = logger.as_deref();
        match level {
            LoggingLevel::Emergency
            | LoggingLevel::Alert
            | LoggingLevel::Critical
            | LoggingLevel::Error => {
                error!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Warning => {
                warn!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Notice | LoggingLevel::Info => {
                info!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
            LoggingLevel::Debug => {
                debug!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger, data
                );
            }
        }
    }
}
