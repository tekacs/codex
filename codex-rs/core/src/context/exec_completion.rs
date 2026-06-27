use codex_protocol::models::ContentItemKind;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use super::ContextualUserFragment;

const EXEC_OUTPUT_TOKENS: usize = 8_000;

#[derive(Clone, Copy)]
pub(crate) enum ExecKind {
    Command,
    Monitor,
}

impl ExecKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Command => "exec-command-completed",
            Self::Monitor => "monitor-command-ended",
        }
    }
}

/// Model-visible completion of a unified exec process that previously yielded.
pub(crate) struct ExecCompletion {
    body: String,
}

impl ExecCompletion {
    pub(crate) fn new(
        kind: ExecKind,
        call_id: &str,
        process_id: i32,
        command: &[String],
        exit_code: i32,
        output: &str,
    ) -> Self {
        let tag = kind.tag();
        let command = command.join(" ");
        let output = truncate_text(output, TruncationPolicy::Tokens(EXEC_OUTPUT_TOKENS));
        Self {
            body: format!(
                "<{tag} call-id=\"{call_id}\" process-id=\"{process_id}\" exit-code=\"{exit_code}\">\n<command>{command}</command>\n<output>\n{output}\n</output>\n</{tag}>"
            ),
        }
    }
}

impl ContextualUserFragment for ExecCompletion {
    fn role(&self) -> &'static str {
        "user"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("exec.completion".to_string())
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
