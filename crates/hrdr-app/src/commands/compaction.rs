use std::sync::Arc;

use hrdr_agent::Agent;
use tokio::sync::Mutex;

/// The shared compaction core (`/compact` and threshold auto-compaction):
/// lock the agent and summarize. `Ok((before, after))` with `before == after`
/// means there was nothing to compact.
pub async fn run_compaction(
    agent: Arc<Mutex<Agent>>,
    instructions: Option<String>,
) -> Result<(usize, usize), String> {
    let mut a = agent.lock().await;
    a.compact(instructions.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// The system line a finished compaction shows — identical in both frontends.
pub fn compaction_message(res: &Result<(usize, usize), String>) -> String {
    match res {
        Ok((before, after)) if before == after => "nothing to compact yet".to_string(),
        Ok((before, after)) => format!(
            "compacted: {before} → {after} messages (summary kept; scrollback above is \
             preserved for you)"
        ),
        Err(e) => format!("[compact failed] {e}"),
    }
}

/// The context-usage token count at which auto-compaction fires:
/// `context_window − reserved` (opencode's reserved model). The reserve is
/// clamped to a quarter of the window so a `reserved` larger than a small
/// model's context still leaves a sane trigger — opencode clamps by the model's
/// max-output tokens; lacking that figure, a quarter-window proxy keeps the
/// trigger from collapsing to 0 (which would compact every turn).
pub fn compaction_trigger(window: u32, reserved: u32) -> u32 {
    window.saturating_sub(reserved.min(window / 4))
}

/// Whether the context usage warrants a proactive compaction before more work.
/// Fires once usage reaches [`compaction_trigger`], shared by both frontends.
/// `enabled` gates it (the `auto_compact` toggle); `last_prompt_tokens` is the
/// latest model call's prompt size.
pub fn should_auto_compact(
    last_prompt_tokens: Option<u32>,
    context_window: Option<u32>,
    reserved: u32,
    enabled: bool,
) -> bool {
    if !enabled {
        return false;
    }
    let (Some(prompt), Some(window)) = (last_prompt_tokens, context_window) else {
        return false;
    };
    window > 0 && prompt >= compaction_trigger(window, reserved)
}
