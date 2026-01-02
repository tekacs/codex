use codex_protocol::models::ContentItemKind;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use super::ContextualUserFragment;

const RESOURCE_UPDATE_TOKENS: usize = 10_000;

/// Model-visible content published by an MCP resource update notification.
pub(crate) struct ResourceUpdate {
    body: String,
}

impl ResourceUpdate {
    pub(crate) fn new(body: String) -> Self {
        Self {
            body: truncate_text(&body, TruncationPolicy::Tokens(RESOURCE_UPDATE_TOKENS)),
        }
    }
}

impl ContextualUserFragment for ResourceUpdate {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("mcp.resource_update".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn body(&self) -> String {
        self.body.clone()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }
}
