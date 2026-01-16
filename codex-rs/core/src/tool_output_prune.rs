use std::collections::HashMap;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_utils_output_truncation::approx_token_count;

const PRUNE_MINIMUM_TOKENS: usize = 20_000;
const PRUNE_PROTECT_TOKENS: usize = 40_000;
const PRUNE_PLACEHOLDER: &str = "[Old tool result content cleared]";

pub(crate) fn prune(items: &[ResponseItem]) -> Vec<ResponseItem> {
    let mut call_names = HashMap::<&str, &str>::new();
    for item in items {
        match item {
            ResponseItem::FunctionCall { call_id, name, .. } => {
                call_names.insert(call_id.as_str(), name.as_str());
            }
            ResponseItem::CustomToolCall { call_id, name, .. } => {
                call_names.insert(call_id.as_str(), name.as_str());
            }
            _ => {}
        }
    }

    let mut tool_output_tokens = 0usize;
    let mut prunable_tokens = 0usize;
    let mut prune_indices = Vec::<usize>::new();
    let mut user_turns = 0usize;

    for (idx, item) in items.iter().enumerate().rev() {
        match item {
            item if is_user_input(item) => {
                user_turns += 1;
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                if user_turns < 2 {
                    continue;
                }
                if output.success == Some(false) {
                    continue;
                }
                let Some(call_id) = call_id.as_deref() else {
                    continue;
                };
                if should_protect_tool(call_names.get(call_id).copied()) {
                    continue;
                }

                let tokens = output
                    .body
                    .to_text()
                    .map(|text| approx_token_count(&text))
                    .unwrap_or(0);
                tool_output_tokens = tool_output_tokens.saturating_add(tokens);
                if tool_output_tokens > PRUNE_PROTECT_TOKENS {
                    prunable_tokens = prunable_tokens.saturating_add(tokens);
                    prune_indices.push(idx);
                }
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                if user_turns < 2 {
                    continue;
                }
                if should_protect_tool(call_names.get(call_id.as_str()).copied()) {
                    continue;
                }

                let tokens = output
                    .body
                    .to_text()
                    .map(|text| approx_token_count(&text))
                    .unwrap_or(0);
                tool_output_tokens = tool_output_tokens.saturating_add(tokens);
                if tool_output_tokens > PRUNE_PROTECT_TOKENS {
                    prunable_tokens = prunable_tokens.saturating_add(tokens);
                    prune_indices.push(idx);
                }
            }
            _ => {}
        }
    }

    if prunable_tokens <= PRUNE_MINIMUM_TOKENS {
        return items.to_vec();
    }

    let mut pruned = items.to_vec();
    for idx in prune_indices {
        match &mut pruned[idx] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                let success = output.success;
                *output = FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(PRUNE_PLACEHOLDER.to_string()),
                    success,
                };
            }
            ResponseItem::CustomToolCallOutput { output, .. } => {
                let success = output.success;
                *output = FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(PRUNE_PLACEHOLDER.to_string()),
                    success,
                };
            }
            _ => {}
        }
    }

    pruned
}

fn is_user_input(item: &ResponseItem) -> bool {
    let ResponseItem::Message {
        role,
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        return false;
    };
    if role != "user" {
        return false;
    }
    internal_chat_message_metadata_passthrough
        .as_ref()
        .and_then(|metadata| metadata.content_item_kinds.as_ref())
        .is_none_or(|kinds| kinds.iter().any(|kind| kind.0.starts_with("user.")))
}

fn should_protect_tool(name: Option<&str>) -> bool {
    // Avoid pruning "apply_patch" outputs by default; those often contain the only durable
    // artifact of what actually changed.
    matches!(name, Some("apply_patch"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn function_call(call_id: &str, name: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: name.to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
    }

    fn function_call_output(call_id: &str, content: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(content.to_string()),
                success: Some(true),
            },
            internal_chat_message_metadata_passthrough: None,
        }
    }

    #[test]
    fn does_not_prune_when_below_minimum() {
        let items = vec![
            user_message("u1"),
            function_call("c1", "shell"),
            function_call_output("c1", "small output"),
            user_message("u2"),
        ];

        let pruned = prune(&items);

        assert_eq!(pruned, items);
    }

    #[test]
    fn prunes_older_tool_outputs_past_protect_window() {
        let big = "word ".repeat(60_000);
        let small = "word ".repeat(10_000);
        let items = vec![
            function_call("c_old", "shell"),
            function_call_output("c_old", &big),
            user_message("u1"),
            function_call("c_new", "shell"),
            function_call_output("c_new", &small),
            user_message("u2"),
        ];

        let pruned = prune(&items);

        let old_output = match &pruned[1] {
            ResponseItem::FunctionCallOutput { output, .. } => output.text_content().unwrap_or(""),
            other => panic!("unexpected item: {other:?}"),
        };
        assert_eq!(old_output, PRUNE_PLACEHOLDER);

        let new_output = match &pruned[4] {
            ResponseItem::FunctionCallOutput { output, .. } => output.text_content().unwrap_or(""),
            other => panic!("unexpected item: {other:?}"),
        };
        assert_eq!(new_output, small);
    }

    #[test]
    fn does_not_prune_apply_patch_outputs() {
        let big = "word ".repeat(60_000);
        let small = "word ".repeat(10_000);
        let items = vec![
            function_call("c_old", "apply_patch"),
            function_call_output("c_old", &big),
            user_message("u1"),
            function_call("c_new", "shell"),
            function_call_output("c_new", &small),
            user_message("u2"),
        ];

        let pruned = prune(&items);

        assert_eq!(pruned, items);
    }
}
