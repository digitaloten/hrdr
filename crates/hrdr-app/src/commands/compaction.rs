use std::sync::Arc;

use hrdr_agent::{Agent, CompactionReason, CompactionReport};
use tokio::sync::Mutex;

/// The shared compaction core for `/compact`: lock the agent and summarize. A
/// report whose `before == after` means there was nothing to compact.
///
/// Works on any agent, main or delegated. Compaction is a *context-window*
/// concern, not a session one: a sub-agent reading its way through a codebase on
/// a small local model fills its window like anything else, and it compacts
/// itself as it goes ([`hrdr_agent::Agent::maybe_self_compact`]) because nothing
/// else is watching its usage.
/// `on_event` takes the summarization calls' [`hrdr_agent::AgentEvent::Usage`]
/// events, so a `/compact` is accounted like a turn. A frontend that drops them
/// is a frontend whose token counters quietly omit its largest model calls.
pub async fn run_compaction<F: FnMut(hrdr_agent::AgentEvent)>(
    agent: Arc<Mutex<Agent>>,
    instructions: Option<String>,
    on_event: &mut F,
) -> Result<CompactionReport, String> {
    let mut a = agent.lock().await;
    a.compact(
        CompactionReason::UserRequested,
        instructions.as_deref(),
        on_event,
    )
    .await
    .map_err(|e| e.to_string())
}

/// The system line a finished compaction shows. The counts and the cache
/// figures come from the report's own renderer, so this line and the agent's
/// self-compaction notices cannot describe the same numbers differently.
pub fn compaction_message(res: &Result<CompactionReport, String>) -> String {
    match res {
        Ok(report) if !report.shrank() => report.notice(),
        Ok(report) => format!(
            "{}\n(summary kept; scrollback above is preserved for you)",
            report.notice()
        ),
        Err(e) => format!("[compact failed] {e}"),
    }
}

/// The context-usage token count at which auto-compaction fires. Re-exported from
/// `hrdr-agent`, which owns the math — the agent compacts itself on the same
/// threshold, and two copies would drift.
pub use hrdr_agent::{compaction_trigger, should_auto_compact};
