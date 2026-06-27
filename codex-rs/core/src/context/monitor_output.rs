use codex_protocol::models::ContentItemKind;

use super::ContextualUserFragment;

/// Model-visible output emitted by a running monitor command.
pub(crate) struct MonitorOutput {
    body: String,
}

impl MonitorOutput {
    pub(crate) fn new(call_id: &str, process_id: i32, command: &[String], output: &str) -> Self {
        let command = command.join(" ");
        Self {
            body: format!(
                "<monitor-command-output call-id=\"{call_id}\" process-id=\"{process_id}\">\n<command>{command}</command>\n<output>\n{output}\n</output>\n</monitor-command-output>"
            ),
        }
    }
}

impl ContextualUserFragment for MonitorOutput {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("exec.monitor_output".to_string())
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
