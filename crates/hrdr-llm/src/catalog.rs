//! The models.dev catalog: per-`provider/model` metadata, used here for one
//! thing — a model's context window.
//!
//! Most OpenAI-compatible endpoints never put the window on the wire. `GET
//! /v1/models` returns `{id, object, created, owned_by}` and nothing else, so
//! [`crate::Client::context_window`] comes back empty and the status bar has no
//! "of Y" to show. The number does exist, just in a catalog rather than an API:
//! <https://models.dev/api.json>, keyed `provider → models → model → limit.context`.
//!
//! Reads are served from a cache file under the XDG cache dir. The catalog is
//! fetched only when that file is missing or older than [`CACHE_TTL`], and a
//! failed fetch falls back to whatever stale copy exists — a context window that
//! is a few days out of date beats none at all. Callers run this off the UI
//! thread; it never blocks a frame.
//!
//! Escape hatches, mirroring opencode's:
//!
//! * `HRDR_MODELS_URL` — fetch the catalog from somewhere else.
//! * `HRDR_MODELS_PATH` — read this file and never fetch (an air-gapped mirror).
//! * `HRDR_DISABLE_MODELS_FETCH` — never fetch; use the cache if one exists.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Where the catalog is fetched from, unless `HRDR_MODELS_URL` says otherwise.
const DEFAULT_URL: &str = "https://models.dev";

/// How old the cache may be before it is refetched. The catalog changes when a
/// provider ships a model, not by the minute.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Give up on a slow catalog rather than delay the probe indefinitely.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The context window for `model`, from the models.dev catalog.
///
/// `provider` is the configured provider name (`opencode-go`, `openrouter`, …)
/// when there is one; it disambiguates a model that several providers serve with
/// different limits. Without it — or when the name isn't in the catalog — every
/// provider is searched and the *smallest* window any of them reports is used:
/// understating the window only compacts earlier than needed, while overstating
/// it overflows the model's real context.
///
/// `None` when the catalog can't be read or doesn't know the model.
pub async fn context_window(provider: Option<&str>, model: &str) -> Option<u32> {
    lookup(&load().await?, provider, model)
}

/// Find `model`'s window in an already-loaded catalog. Pure, so the resolution
/// rules are testable without a cache or a network.
fn lookup(catalog: &Value, provider: Option<&str>, model: &str) -> Option<u32> {
    let window = |p: &Value| -> Option<u32> {
        p.get("models")?
            .get(model)?
            .get("limit")?
            .get("context")?
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
    };
    // The configured provider's own answer, when the catalog has one.
    if let Some(name) = provider
        && let Some(p) = catalog.get(name)
        && let Some(n) = window(p)
    {
        return Some(n);
    }
    // Otherwise the most conservative window offered for this model id.
    catalog.as_object()?.values().filter_map(window).min()
}

/// The cache file, `$XDG_CACHE_HOME/hrdr/models.json`.
fn cache_path() -> Option<PathBuf> {
    Some(hjkl_xdg::cache_dir("hrdr").ok()?.join("models.json"))
}

/// Whether `path` exists and was written within [`CACHE_TTL`].
fn is_fresh(path: &std::path::Path, ttl: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .is_some_and(|age| age < ttl)
}

/// The catalog: a pinned file, else a fresh cache, else a fetch (cached on
/// success), else whatever stale cache is on disk.
async fn load() -> Option<Value> {
    let read = |p: &std::path::Path| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };

    // A pinned file is authoritative: read it, never fetch, never cache.
    if let Some(pinned) = std::env::var_os("HRDR_MODELS_PATH") {
        return read(std::path::Path::new(&pinned));
    }

    let cache = cache_path();
    if let Some(p) = &cache
        && is_fresh(p, CACHE_TTL)
        && let Some(v) = read(p)
    {
        return Some(v);
    }
    let stale = cache.as_deref().and_then(read);
    if std::env::var_os("HRDR_DISABLE_MODELS_FETCH").is_some() {
        return stale;
    }

    match fetch().await {
        Some(v) => {
            if let Some(p) = &cache {
                write_cache(p, &v);
            }
            Some(v)
        }
        // The network is down or models.dev moved: a stale window beats none.
        None => stale,
    }
}

/// GET `{base}/api.json`, decoded. `None` on any error — the catalog is a
/// nicety, never a reason to fail a session.
async fn fetch() -> Option<Value> {
    let base = std::env::var("HRDR_MODELS_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!("hrdr/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{}/api.json", base.trim_end_matches('/')))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// Write the catalog to `path` via a temporary file in the same directory, so a
/// crash or a concurrent hrdr can't leave a half-written cache behind. Failure
/// is ignored: the caller already has the value in hand.
fn write_cache(path: &std::path::Path, v: &Value) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if serde_json::to_string(v)
        .ok()
        .and_then(|s| std::fs::write(&tmp, s).ok())
        .is_some()
    {
        let _ = std::fs::rename(&tmp, path);
    }
    let _ = std::fs::remove_file(&tmp);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> Value {
        json!({
            "opencode-go": { "models": {
                "deepseek-v4-flash": { "limit": { "context": 1_000_000, "output": 384_000 } },
            }},
            "cortecs": { "models": {
                "deepseek-v4-flash": { "limit": { "context": 1_048_576 } },
            }},
            "openai": { "models": {
                "gpt-5": { "limit": { "context": 400_000 } },
                // A model whose entry carries no limit at all.
                "weird": { "id": "weird" },
            }},
        })
    }

    /// The configured provider's own number wins — the same model is served with
    /// different windows by different providers.
    #[test]
    fn the_named_provider_decides() {
        let c = catalog();
        assert_eq!(
            lookup(&c, Some("opencode-go"), "deepseek-v4-flash"),
            Some(1_000_000)
        );
        assert_eq!(
            lookup(&c, Some("cortecs"), "deepseek-v4-flash"),
            Some(1_048_576)
        );
    }

    /// With no provider — or one the catalog doesn't carry — take the smallest
    /// window on offer: compacting early is recoverable, overflowing isn't.
    #[test]
    fn without_a_provider_the_smallest_window_wins() {
        let c = catalog();
        assert_eq!(lookup(&c, None, "deepseek-v4-flash"), Some(1_000_000));
        assert_eq!(
            lookup(&c, Some("not-in-catalog"), "deepseek-v4-flash"),
            Some(1_000_000),
            "an unknown provider falls back to the scan"
        );
        // The named provider knows the model but not its window: scan instead.
        assert_eq!(lookup(&c, Some("openai"), "weird"), None);
    }

    /// An unknown model, an empty catalog, and a zero window are all `None`.
    #[test]
    fn unknown_models_and_junk_windows_are_none() {
        let c = catalog();
        assert_eq!(lookup(&c, Some("openai"), "no-such-model"), None);
        assert_eq!(lookup(&json!({}), None, "gpt-5"), None);
        assert_eq!(lookup(&json!([1, 2]), None, "gpt-5"), None, "not an object");
        let zero = json!({"p": {"models": {"m": {"limit": {"context": 0}}}}});
        assert_eq!(lookup(&zero, Some("p"), "m"), None, "0 is not a window");
    }

    /// A file younger than the TTL is fresh; an older one isn't, and a missing
    /// one never is.
    #[test]
    fn cache_freshness_follows_the_file_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("models.json");
        assert!(!is_fresh(&p, CACHE_TTL), "a missing cache is never fresh");

        std::fs::write(&p, "{}").unwrap();
        assert!(is_fresh(&p, CACHE_TTL), "just written");
        // Zero TTL: anything already on disk is stale.
        assert!(!is_fresh(&p, Duration::ZERO));
    }

    /// The cache lands atomically and leaves no `.tmp` behind.
    #[test]
    fn writing_the_cache_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("models.json");
        write_cache(&p, &catalog());

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            lookup(&back, Some("opencode-go"), "deepseek-v4-flash"),
            Some(1_000_000)
        );
        assert!(
            !p.with_extension("json.tmp").exists(),
            "temp file cleaned up"
        );
    }
}
