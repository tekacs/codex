use crate::function_tool::FunctionCallError;
use crate::safety::SafetyCheck;
use crate::safety::assess_patch_safety;
use crate::session::turn_context::TurnContext;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DIFFTASTIC_SPLIT_MARKER: &str = "\n\x1eCODEX_DIFFTASTIC\x1e\n";

pub(crate) enum InternalApplyPatchInvocation {
    /// The `apply_patch` call was handled programmatically, without any sort
    /// of sandbox, because the user explicitly approved it. This is the
    /// result to use with the `shell` function call that contained `apply_patch`.
    Output(Result<String, FunctionCallError>),

    /// The `apply_patch` call was approved, either automatically because it
    /// appears that it should be allowed based on the user's sandbox policy
    /// *or* because the user explicitly approved it. The runtime realizes the
    /// patch through the selected environment filesystem.
    DelegateToRuntime(ApplyPatchRuntimeInvocation),
}

#[derive(Debug)]
pub(crate) struct ApplyPatchRuntimeInvocation {
    pub(crate) action: ApplyPatchAction,
    pub(crate) auto_approved: bool,
    pub(crate) exec_approval_requirement: ExecApprovalRequirement,
}

pub(crate) async fn apply_patch(
    turn_context: &TurnContext,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    action: ApplyPatchAction,
) -> InternalApplyPatchInvocation {
    match assess_patch_safety(
        &action,
        turn_context.approval_policy.value(),
        &turn_context.permission_profile(),
        file_system_sandbox_policy,
        &turn_context.cwd,
        turn_context.windows_sandbox_level,
    ) {
        SafetyCheck::AutoApprove {
            user_explicitly_approved,
            ..
        } => InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
            action,
            auto_approved: !user_explicitly_approved,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        }),
        SafetyCheck::AskUser => {
            // Delegate the approval prompt (including cached approvals) to the
            // tool runtime, consistent with how shell/unified_exec approvals
            // are orchestrator-driven.
            InternalApplyPatchInvocation::DelegateToRuntime(ApplyPatchRuntimeInvocation {
                action,
                auto_approved: false,
                exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
                    reason: None,
                    proposed_execpolicy_amendment: None,
                },
            })
        }
        SafetyCheck::Reject { reason } => InternalApplyPatchInvocation::Output(Err(
            FunctionCallError::RespondToModel(format!("patch rejected: {reason}")),
        )),
    }
}

pub(crate) fn convert_apply_patch_to_protocol(
    action: &ApplyPatchAction,
) -> HashMap<PathBuf, FileChange> {
    let changes = action.changes();
    let mut result = HashMap::with_capacity(changes.len());
    let difft = which::which("difft").ok();
    for (path, change) in changes {
        let protocol_change = match change {
            ApplyPatchFileChange::Add { content } => FileChange::Add {
                content: content.clone(),
            },
            ApplyPatchFileChange::Delete { content } => FileChange::Delete {
                content: content.clone(),
            },
            ApplyPatchFileChange::Update {
                unified_diff,
                move_path,
                new_content,
            } => FileChange::Update {
                unified_diff: maybe_embed_difftastic_render(
                    difft.as_ref(),
                    action.cwd.as_path(),
                    path.as_path(),
                    move_path.as_ref(),
                    unified_diff,
                    new_content,
                ),
                move_path: move_path.clone(),
            },
        };
        result.insert(path.clone(), protocol_change);
    }
    result
}

fn maybe_embed_difftastic_render(
    difft: Option<&PathBuf>,
    cwd: &Path,
    path: &Path,
    move_path: Option<&PathBuf>,
    unified_diff: &str,
    new_content: &str,
) -> String {
    let Some(difft) = difft else {
        return unified_diff.to_owned();
    };

    let Some(rendered) = try_render_difftastic(difft, cwd, path, move_path, new_content) else {
        return unified_diff.to_owned();
    };

    format!("{unified_diff}{DIFFTASTIC_SPLIT_MARKER}{rendered}")
}

fn try_render_difftastic(
    difft: &Path,
    cwd: &Path,
    path: &Path,
    move_path: Option<&PathBuf>,
    new_content: &str,
) -> Option<String> {
    let old_content = std::fs::read_to_string(path).ok()?;

    let suffix = move_path
        .and_then(|p| p.extension())
        .or_else(|| path.extension())
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();

    let dir = tempfile::tempdir().ok()?;
    let mut old_file = tempfile::Builder::new()
        .prefix("codex-difftastic-old-")
        .suffix(&suffix)
        .tempfile_in(dir.path())
        .ok()?;
    let mut new_file = tempfile::Builder::new()
        .prefix("codex-difftastic-new-")
        .suffix(&suffix)
        .tempfile_in(dir.path())
        .ok()?;

    use std::io::Write as _;
    old_file.write_all(old_content.as_bytes()).ok()?;
    new_file.write_all(new_content.as_bytes()).ok()?;

    let output = Command::new(difft)
        .args(["--display", "inline", "--color", "always"])
        .arg(old_file.path())
        .arg(new_file.path())
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() {
        return None;
    }

    let old_display = display_path_for_difftastic(cwd, path);
    let new_display = display_path_for_difftastic(cwd, move_path.map_or(path, PathBuf::as_path));

    // Difftastic prints the file paths it received. Replace our temp file paths
    // with the user-facing paths so the output is readable in the patch summary.
    let old_tmp = old_file.path().display().to_string();
    let new_tmp = new_file.path().display().to_string();
    Some(
        stdout
            .replace(&old_tmp, &old_display)
            .replace(&new_tmp, &new_display),
    )
}

fn display_path_for_difftastic(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
#[path = "apply_patch_tests.rs"]
mod tests;
