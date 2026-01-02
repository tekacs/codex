use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientInfo;
use rmcp::model::ElicitRequestParams;
use rmcp::model::ElicitResult;
#[allow(deprecated)]
use rmcp::model::LoggingLevel;
#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::rmcp_client::Elicitation;
use crate::rmcp_client::HandleResourceUpdate;
use crate::rmcp_client::SendElicitation;

#[derive(Clone)]
pub(crate) struct LoggingClientHandler {
    client_info: ClientInfo,
    send_elicitation: Arc<SendElicitation>,
    resource_update_handler: Option<HandleResourceUpdate>,
}

impl LoggingClientHandler {
    pub(crate) fn new(
        client_info: ClientInfo,
        send_elicitation: SendElicitation,
        resource_update_handler: Option<HandleResourceUpdate>,
    ) -> Self {
        Self {
            client_info,
            send_elicitation: Arc::new(send_elicitation),
            resource_update_handler,
        }
    }
}

impl ClientHandler for LoggingClientHandler {
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        (self.send_elicitation)(context.id, Elicitation::Mcp(request))
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
            "MCP server cancelled request (request_id: {:?}, reason: {:?})",
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
            .and_then(|info| info.server_info.as_ref().map(|server| server.name.clone()))
            .unwrap_or_else(|| "unknown".to_string());
        info!(server = server_name, uri, "MCP server resource updated");

        let Some(handle_resource_update) = self.resource_update_handler.clone() else {
            return;
        };
        let resource = match context
            .peer
            .read_resource(rmcp::model::ReadResourceRequestParams::new(uri.clone()))
            .await
        {
            Ok(result) => {
                let parts = result
                    .contents
                    .into_iter()
                    .map(|content| match content {
                        rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                            format!(
                                "<resource server=\"{server_name}\" uri=\"{uri}\">\n{text}\n</resource>"
                            )
                        }
                        rmcp::model::ResourceContents::BlobResourceContents {
                            blob,
                            mime_type,
                            ..
                        } => {
                            let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                            format!(
                                "<resource server=\"{server_name}\" uri=\"{uri}\" type=\"blob\" mime-type=\"{mime}\" size=\"{}\">\n[Binary resource - use read_mcp_resource to retrieve if needed]\n</resource>",
                                blob.len()
                            )
                        }
                        _ => format!(
                            "<resource server=\"{server_name}\" uri=\"{uri}\">\n[unsupported resource content]\n</resource>"
                        ),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("<resource-updated server=\"{server_name}\" uri=\"{uri}\" />\n{parts}")
            }
            Err(error) => {
                warn!(
                    server = server_name,
                    uri,
                    ?error,
                    "failed to read updated MCP resource"
                );
                format!(
                    "<resource-updated server=\"{server_name}\" uri=\"{uri}\" />\n<resource server=\"{server_name}\" uri=\"{uri}\">\n[error reading resource: {error:?}]\n</resource>"
                )
            }
        };
        handle_resource_update(resource).await;
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

    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let LoggingMessageNotificationParam {
            level,
            logger,
            data,
            ..
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
