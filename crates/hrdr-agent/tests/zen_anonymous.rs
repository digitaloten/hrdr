//! OpenCode Zen's free models work with no login, so hrdr offers them with no
//! login — and offers nothing else.
//!
//! Zen's gateway reads the literal API key `public` as "anonymous caller" and
//! serves it every zero-cost model, IP-rate-limited; a priced model answers `401
//! Missing API key`. opencode's own client does exactly this (`if (!hasKey)
//! provider.request.body.apiKey = "public"`, then it disables every row priced
//! above zero), and this pins hrdr's half of the arrangement:
//!
//! * the provider reports [`ProviderAuthState::Anonymous`] rather than `Missing`,
//!   so it survives the `/model` picker's "is this set up?" gate;
//! * the wire key is `public`, while `resolve_api_key` — the question "does this
//!   user hold a credential?" — still answers `None`;
//! * the picker lists the free models and *only* those, because offering a priced
//!   row to a logged-out user is offering something that cannot run.
//!
//! ONE test in its own binary, on purpose. It mutates provider key env vars —
//! which are process-wide and read by every other test in the crate — so there is
//! deliberately no second test here to race the `set_var` with.

extern crate hrdr_test_support;

use hrdr_agent::{
    AgentConfig, ProviderAuthState, builtin_provider, model_choices, provider_auth_state,
    public_api_key, resolve_api_key, resolve_api_key_or_public,
};

/// A models.dev catalog for `opencode` with one free model and one priced one,
/// plus a priced `opencode-go` entry (Zen's sibling has no anonymous tier).
///
/// Also clears the provider key vars: they are read from the process environment,
/// and a developer with `OPENCODE_API_KEY` exported would otherwise be testing the
/// authenticated path.
fn logged_out_with_catalog() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("models.json");
    std::fs::write(
        &path,
        r#"{
          "opencode": {"name": "OpenCode Zen", "models": {
            "grok-code":  {"name": "Grok Code",  "cost": {"input": 0, "output": 0}},
            "claude-opus-5": {"name": "Claude Opus 5", "cost": {"input": 5, "output": 25}},
            "unpriced":   {"name": "Unpriced"}
          }},
          "opencode-go": {"name": "OpenCode Go", "models": {
            "deepseek-v4-pro": {"name": "DeepSeek V4 Pro", "cost": {"input": 1, "output": 2}}
          }}
        }"#,
    )
    .expect("the catalog is written");
    // SAFETY: single-threaded test binary — one test, no thread has been spawned.
    unsafe {
        std::env::set_var("HRDR_MODELS_PATH", &path);
        std::env::remove_var("OPENCODE_API_KEY");
        std::env::remove_var("HRDR_API_KEY");
    }
    dir
}

fn zen_rows(cfg: &AgentConfig) -> Vec<String> {
    let mut ids: Vec<String> = model_choices(cfg, None)
        .into_iter()
        .filter(|c| c.provider == "zen")
        .map(|c| c.model)
        .collect();
    ids.sort();
    ids
}

#[test]
fn zen_serves_its_free_models_anonymously_and_its_priced_ones_only_to_an_account() {
    let _catalog = logged_out_with_catalog();
    let cfg = AgentConfig::default();
    let zen = builtin_provider("zen").expect("a built-in");

    // No credential of any kind — and yet the provider is usable.
    assert_eq!(
        resolve_api_key("zen", &zen, None, None),
        None,
        "nothing is stored, so the user holds no credential"
    );
    assert_eq!(
        provider_auth_state("zen", &zen, None, None),
        ProviderAuthState::Anonymous,
        "`Missing` here is what used to hide Zen from a logged-out picker"
    );
    assert_eq!(
        resolve_api_key_or_public("zen", &zen, None, None).as_deref(),
        Some("public"),
        "the wire key Zen reads as `no account`"
    );

    // The picker offers the free model, and nothing that would 401.
    assert_eq!(
        zen_rows(&cfg),
        vec!["grok-code".to_string()],
        "free only: the priced model is unrunnable, and an UNPRICED one is \
         unknown — which is not the same as free"
    );

    // Zen's sibling has no anonymous tier, so it stays out entirely.
    let go = builtin_provider("go").expect("a built-in");
    assert_eq!(
        provider_auth_state("go", &go, None, None),
        ProviderAuthState::Missing
    );
    assert!(public_api_key("go", &go).is_none());
    assert!(
        !model_choices(&cfg, None).iter().any(|c| c.provider == "go"),
        "a provider with no credential and no anonymous tier is not offered"
    );

    // ── A real key outranks the anonymous tier ──────────────────────────────
    // SAFETY: the only test in this binary, and no thread has been spawned.
    unsafe { std::env::set_var("OPENCODE_API_KEY", "sk-real") };
    assert_eq!(
        provider_auth_state("zen", &zen, None, None),
        ProviderAuthState::Key,
        "a real key outranks the anonymous tier"
    );
    assert_eq!(
        resolve_api_key_or_public("zen", &zen, None, None).as_deref(),
        Some("sk-real"),
        "and is never swapped for `public`"
    );
    assert_eq!(
        zen_rows(&AgentConfig::default()),
        vec![
            "claude-opus-5".to_string(),
            "grok-code".to_string(),
            "unpriced".to_string()
        ],
        "an authenticated account sees the whole catalog, priced rows included"
    );

    unsafe { std::env::remove_var("OPENCODE_API_KEY") };

    // ── The anonymous key belongs to the BUILT-IN preset ────────────────────
    // A `[providers.zen]` entry points at an endpoint of the user's own, which has
    // never heard of `public`. Handing it a key it did not ask for is the same
    // class of mistake as handing a custom shadow the account's OAuth bearer.
    let mut cfg = AgentConfig::default();
    cfg.providers.insert(
        "zen".to_string(),
        hrdr_agent::ProviderConfig {
            base_url: "http://localhost:9099/v1".to_string(),
            key_env: None,
            api_key: None,
            model: None,
            remote: None,
            context_window: None,
            headers: std::collections::HashMap::new(),
            api_version: None,
        },
    );
    let shadow = cfg.resolve_provider("zen").expect("the user's entry wins");
    assert_eq!(shadow.kind, hrdr_agent::ResolvedProviderKind::Custom);
    assert!(public_api_key("zen", &shadow).is_none());
    assert_eq!(resolve_api_key_or_public("zen", &shadow, None, None), None);
}
