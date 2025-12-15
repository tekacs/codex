use crate::function_tool::FunctionCallError;
use crate::safety::PatchSandboxRoute;
use crate::safety::SafetyCheck;
use crate::safety::assess_patch_safety;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnEnvironment;
use crate::tools::sandboxing::ExecApprovalRequirement;
use codex_apply_patch::ApplyPatchAction;
use codex_apply_patch::ApplyPatchFileChange;
use codex_protocol::permissions::FileSystemSandboxPolicyContext;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DIFFTASTIC_SPLIT_MARKER: &str = "\n\x1eCODEX_DIFFTASTIC\x1e\n";

#[derive(Debug)]
pub(crate) struct ApplyPatchRuntimeInvocation {
    pub(crate) action: ApplyPatchAction,
    pub(crate) auto_approved: bool,
    pub(crate) exec_approval_requirement: ExecApprovalRequirement,
}

pub(crate) fn prepare_apply_patch(
    step_context: &StepContext,
    turn_environment: &TurnEnvironment,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    context: &FileSystemSandboxPolicyContext<'_>,
    sandbox_route: PatchSandboxRoute,
    action: ApplyPatchAction,
) -> Result<ApplyPatchRuntimeInvocation, FunctionCallError> {
    match assess_patch_safety(
        &action,
        step_context.settings.approval_policy(),
        turn_environment.permission_profile(),
        file_system_sandbox_policy,
        context,
        sandbox_route,
    ) {
        SafetyCheck::AutoApprove => Ok(ApplyPatchRuntimeInvocation {
            action,
            auto_approved: true,
            exec_approval_requirement: ExecApprovalRequirement::Skip {
                bypass_sandbox: false,
                proposed_execpolicy_amendment: None,
            },
        }),
        SafetyCheck::AskUser => {
            // Delegate the approval prompt (including cached approvals) to the
            // tool runtime, consistent with how shell/unified_exec approvals
            // are orchestrator-driven.
            Ok(ApplyPatchRuntimeInvocation {
                action,
                auto_approved: false,
                exec_approval_requirement: ExecApprovalRequirement::NeedsApproval {
                    reason: None,
                    proposed_execpolicy_amendment: None,
                },
            })
        }
        SafetyCheck::Reject { reason } => Err(FunctionCallError::RespondToModel(format!(
            "patch rejected: {reason}"
        ))),
    }
}

pub(crate) fn convert_apply_patch_to_protocol(
    action: &ApplyPatchAction,
) -> HashMap<PathBuf, FileChange> {
    let mut result = HashMap::with_capacity(action.changes().len());
    let difft = which::which("difft").ok();
    for (path, change) in action.changes() {
        let protocol_change = match change {
            ApplyPatchFileChange::Add { content, .. } => FileChange::Add {
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
                    &action.cwd,
                    path,
                    move_path.as_ref(),
                    unified_diff,
                    new_content,
                ),
                move_path: move_path.as_ref().map(PathUri::to_path_buf),
            },
        };
        // TODO(anp): Carry PathUri through patch protocol events once app-server and rollout
        // compatibility no longer require path-flavored strings.
        result.insert(path.to_path_buf(), protocol_change);
    }
    result
}

fn maybe_embed_difftastic_render(
    difft: Option<&PathBuf>,
    cwd: &PathUri,
    path: &PathUri,
    move_path: Option<&PathUri>,
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
    cwd: &PathUri,
    path: &PathUri,
    move_path: Option<&PathUri>,
    new_content: &str,
) -> Option<String> {
    let cwd = cwd.to_abs_path().ok()?;
    let cwd = cwd.as_path();

    let path = path.to_abs_path().ok()?.into_path_buf();
    let move_path = move_path.map(PathUri::to_path_buf);
    let old_content = std::fs::read_to_string(&path).ok()?;

    let suffix = move_path
        .as_ref()
        .and_then(|path| path.extension())
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

    let old_display = display_path_for_difftastic(cwd, &path);
    let new_display = display_path_for_difftastic(cwd, move_path.as_ref().unwrap_or(&path));

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
