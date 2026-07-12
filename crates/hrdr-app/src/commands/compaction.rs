use std::sync::Arc;

use hrdr_agent::Agent;
use tokio::sync::Mutex;

/// The shared compaction core (`/compact` and threshold auto-compaction):
/// lock the agent and summarize. `Ok((before, after))` with `before == after`
/// means there was nothing to compact.
///
/// Works on any agent, main or delegated. Compaction is a *context-window*
/// concern, not a session one: a sub-agent reading its way through a codebase on
/// a small local model fills its window like anything else, and it compacts
/// itself as it goes ([`hrdr_agent::Agent::maybe_self_compact`]) because nothing
/// else is watching its usage.
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

/// The context-usage token count at which auto-compaction fires. Re-exported from
/// `hrdr-agent`, which owns the math — the agent compacts itself on the same
/// threshold, and two copies would drift.
pub use hrdr_agent::{compaction_trigger, should_auto_compact};
