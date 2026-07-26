//! **Fail fast on a typo'd model.** The pre-flight, end to end, against a pinned
//! models.dev catalog: the notice an `Agent` queues at construction and on every
//! identity change, and the silence it keeps when nothing local can judge the model.
//!
//! Its own integration binary because it pins the catalog with a process-global env
//! var (`HRDR_MODELS_PATH`), which would race the library's unit tests — several of
//! which read the same catalog. One test, not several, for the same reason: cargo runs
//! a binary's tests on parallel threads.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use hrdr_agent::{Agent, AgentConfig, ModelRef, ProviderConfig};

fn cfg(model: &str) -> AgentConfig {
    AgentConfig {
        model: model.parse::<ModelRef>().expect("an identity"),
        api_key: Some("k".to_string()),
        subagents: false,
        memory: false,
        ..Default::default()
    }
}

#[test]
fn the_preflight_flags_a_typod_model_at_construction_and_on_every_switch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api.json");
    std::fs::write(
        &path,
        r#"{
             "anthropic": {"models": {
                 "claude-sonnet-4-5": {"name": "Claude Sonnet 4.5"},
                 "claude-opus-4-8":   {"name": "Claude Opus 4.8"}
             }},
             "openai": {"models": {"gpt-5": {"name": "GPT-5"}}}
           }"#,
    )
    .unwrap();
    // SAFETY: this binary runs only this test, and nothing else writes these. The dead
    // fetch address means an ignored pin fails the test instead of silently reaching
    // the real models.dev.
    unsafe {
        std::env::set_var("HRDR_MODELS_PATH", &path);
        std::env::set_var("HRDR_MODELS_URL", "http://127.0.0.1:1");
    }

    // (1) A typo on a provider whose model set we hold: flagged at CONSTRUCTION, before
    //     a request is ever sent — and the id they meant is named, so the fix is one
    //     read away rather than another guess.
    let mut agent = Agent::new(cfg("claude://claude-sonet-4-5")).unwrap();
    let notices = agent.take_pending_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("model 'claude-sonet-4-5' isn't in provider 'claude's known catalog"),
        "{notices:?}"
    );
    assert!(
        notices[0].contains("Closest known: 'claude-sonnet-4-5'."),
        "{notices:?}"
    );
    // Taken, not copied.
    assert!(agent.take_pending_notices().is_empty());

    // (2) The switch path gets the same check — `adopt_resolved` is the single writer of
    //     the agent's identity, so nothing can move it without being pre-flighted.
    agent
        .set_model_ref("openai://gpt-5-turbonium".parse().unwrap())
        .unwrap();
    let notices = agent.take_pending_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("model 'gpt-5-turbonium' isn't in provider 'openai's known catalog"),
        "{notices:?}"
    );
    assert!(
        notices[0].contains("Closest known: 'gpt-5'."),
        "{notices:?}"
    );

    // (3) A model that IS in the catalog is silent — at construction and on a switch.
    let mut agent = Agent::new(cfg("claude://claude-opus-4-8")).unwrap();
    assert!(agent.take_pending_notices().is_empty(), "an exact id");
    agent
        .set_model_ref("openai://gpt-5".parse().unwrap())
        .unwrap();
    assert!(
        agent.take_pending_notices().is_empty(),
        "…and switching to another exact id"
    );

    // (4) Nothing local can judge these, so nothing is said: a provider models.dev has
    //     no key for (`local`), one this index does not carry (`openrouter`), and a
    //     custom `[providers.*]` endpoint — which may legitimately serve any id at all.
    for spec in [
        "local://whatever-i-serve",
        "openrouter://deepseek/deepseek-v4",
    ] {
        let mut agent = Agent::new(cfg(spec)).unwrap();
        assert!(agent.take_pending_notices().is_empty(), "{spec}");
    }
    let mut custom = cfg("mygateway://house-blend-v9");
    custom.providers.insert(
        "mygateway".to_string(),
        ProviderConfig {
            base_url: "https://gw.internal/v1".to_string(),
            api_key: Some("k".to_string()),
            ..Default::default()
        },
    );
    let mut agent = Agent::new(custom).unwrap();
    assert!(
        agent.take_pending_notices().is_empty(),
        "a custom endpoint's model list is nobody's to know"
    );
}
