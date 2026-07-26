//! Authentication: token generation, argon2 verify, rate limiter, and the
//! verification functions the server calls inline on each protected route.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use base64::Engine;
use rand::RngExt;
use subtle::ConstantTimeEq;

use crate::config::{AuthMode, WebConfig};

/// Shared auth state, accessible via `AppState.auth`.
#[derive(Clone)]
pub struct AuthState {
    pub mode: AuthMode,
    pub basic_user: Option<String>,
    pub basic_password_hash: Option<String>,
    pub token: Option<String>,
    pub rate_limiter: Arc<Mutex<HashMap<IpAddr, Vec<Instant>>>>,
}

impl AuthState {
    pub fn from_config(cfg: &WebConfig) -> Self {
        let token = if cfg.auth == AuthMode::Token {
            let mut rng = rand::rng();
            let bytes: [u8; 32] = rng.random();
            Some(base64_url_no_pad(&bytes))
        } else {
            None
        };

        Self {
            mode: cfg.auth,
            basic_user: cfg.basic_user.clone(),
            basic_password_hash: cfg.basic_password_hash.clone(),
            token,
            rate_limiter: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn token_str(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

// ── verification ───────────────────────────────────────────────────────────

/// Verify a Bearer token or `?token=` query parameter.
pub fn verify_token(
    headers: &HeaderMap,
    query: &HashMap<String, String>,
    token: Option<&str>,
) -> bool {
    let Some(token) = token else {
        return false;
    };
    if let Some(auth_val) = headers.get(header::AUTHORIZATION)
        && let Ok(auth_str) = auth_val.to_str()
        && let Some(bearer) = auth_str.strip_prefix("Bearer ")
    {
        return constant_time_eq(bearer.as_bytes(), token.as_bytes());
    }
    if let Some(qt) = query.get("token") {
        return constant_time_eq(qt.as_bytes(), token.as_bytes());
    }
    false
}

/// Verify HTTP Basic auth against the configured user + argon2 hash.
pub fn verify_basic(headers: &HeaderMap, user: Option<&str>, hash: Option<&str>) -> bool {
    let (Some(user), Some(hash)) = (user, hash) else {
        return false;
    };
    let Some(auth_val) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(auth_str) = auth_val.to_str() else {
        return false;
    };
    let Some(encoded) = auth_str.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(creds) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((u, p)) = creds.split_once(':') else {
        return false;
    };
    if !constant_time_eq(u.as_bytes(), user.as_bytes()) {
        return false;
    }
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(p.as_bytes(), &parsed_hash)
        .is_ok()
}

// ── rate limiter ───────────────────────────────────────────────────────────

/// Check the rate limiter: max 10 failures per minute per IP.
pub fn check_rate_limit(auth: &AuthState, ip: Option<IpAddr>) -> bool {
    let Some(ip) = ip else {
        return true; // unknown IP, allow
    };
    let mut guard = auth.rate_limiter.lock().unwrap();
    let entry = guard.entry(ip).or_default();
    let now = Instant::now();
    entry.retain(|t| now.duration_since(*t).as_secs() < 60);
    entry.len() < 10
}

/// Record a failed auth attempt.
pub fn rate_limit_record(auth: &AuthState, ip: Option<IpAddr>) {
    let Some(ip) = ip else {
        return;
    };
    let mut guard = auth.rate_limiter.lock().unwrap();
    let entry = guard.entry(ip).or_default();
    entry.push(Instant::now());
    let now = Instant::now();
    entry.retain(|t| now.duration_since(*t).as_secs() < 60);
}

// ── client IP extraction ───────────────────────────────────────────────────

/// Extract the client's IP from the request headers (X-Forwarded-For).
pub fn extract_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    if let Some(xff) = headers.get("x-forwarded-for")
        && let Ok(xff_str) = xff.to_str()
        && let Some(ip) = xff_str.split(',').next().map(|s| s.trim())
        && let Ok(addr) = ip.parse::<IpAddr>()
    {
        return Some(addr);
    }
    None
}

// ── WS Origin check (CSRF hardening) ───────────────────────────────────────

/// Reject if an Origin header is present and its host is neither the request
/// Host nor localhost.
pub fn check_ws_origin(origin: Option<&str>, host: Option<&str>) -> Result<(), StatusCode> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let Some(origin_host) = extract_host_from_url(origin) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if origin_host == "localhost" || origin_host == "127.0.0.1" || origin_host == "[::1]" {
        return Ok(());
    }
    if let Some(host) = host
        && host.split(':').next().unwrap_or(host) == origin_host
    {
        return Ok(());
    }
    Err(StatusCode::FORBIDDEN)
}

fn extract_host_from_url(url: &str) -> Option<String> {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    without_scheme
        .split('/')
        .next()?
        .split(':')
        .next()
        .map(|s| s.to_string())
}

// ── utilities ──────────────────────────────────────────────────────────────

/// Generate an argon2id PHC hash string from a password (for `--hash-password`).
#[allow(clippy::result_large_err)]
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let mut rng = rand::rng();
    let salt_bytes: [u8; 16] = rng.random();
    let salt = SaltString::encode_b64(&salt_bytes).unwrap();
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_verify_matches() {
        let token = "my-secret-token-12345";
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer my-secret-token-12345".parse().unwrap(),
        );
        let query = HashMap::new();
        assert!(verify_token(&headers, &query, Some(token)));
    }

    #[test]
    fn token_verify_rejects_wrong() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer wrong-token".parse().unwrap());
        let query = HashMap::new();
        assert!(!verify_token(&headers, &query, Some("good-token")));
    }

    #[test]
    fn token_verify_query_param() {
        let headers = HeaderMap::new();
        let mut query = HashMap::new();
        query.insert("token".into(), "abc123".into());
        assert!(verify_token(&headers, &query, Some("abc123")));
    }

    #[test]
    fn rate_limiter_locks_out_after_10() {
        let auth = AuthState {
            mode: AuthMode::Token,
            basic_user: None,
            basic_password_hash: None,
            token: None,
            rate_limiter: Arc::new(Mutex::new(HashMap::new())),
        };
        let ip = Some("1.2.3.4".parse().unwrap());
        for _ in 0..10 {
            assert!(check_rate_limit(&auth, ip));
            rate_limit_record(&auth, ip);
        }
        assert!(!check_rate_limit(&auth, ip));
    }

    #[test]
    fn hash_password_produces_valid_hash() {
        let hash = hash_password("test123").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        let parsed = PasswordHash::new(&hash).unwrap();
        Argon2::default()
            .verify_password("test123".as_bytes(), &parsed)
            .unwrap();
    }

    #[test]
    fn ws_origin_rejects_foreign_host() {
        assert!(check_ws_origin(Some("https://evil.com"), Some("myapp.local:9911")).is_err());
    }

    #[test]
    fn ws_origin_allows_localhost() {
        assert!(check_ws_origin(Some("http://127.0.0.1:9911"), None).is_ok());
        assert!(check_ws_origin(Some("http://localhost:8080"), None).is_ok());
    }

    #[test]
    fn ws_origin_allows_matching_host() {
        assert!(
            check_ws_origin(Some("https://myapp.local:9911"), Some("myapp.local:9911")).is_ok()
        );
    }
}
