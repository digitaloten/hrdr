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
use rusqlite::Connection;
use subtle::ConstantTimeEq;

use crate::config::{AuthMode, WebConfig};

/// Shared auth state, accessible via `AppState.auth`.
#[derive(Clone)]
pub struct AuthState {
    pub mode: AuthMode,
    pub basic_user: Option<String>,
    pub basic_password_hash: Option<String>,
    pub token: Option<String>,
    pub cookie_secret: Arc<[u8; 32]>,
    pub users_db: Arc<Mutex<Option<Connection>>>,
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

        let mut rng = rand::rng();
        let cookie_secret: [u8; 32] = rng.random();

        Self {
            mode: cfg.auth,
            basic_user: cfg.basic_user.clone(),
            basic_password_hash: cfg.basic_password_hash.clone(),
            token,
            cookie_secret: Arc::new(cookie_secret),
            users_db: Arc::new(Mutex::new(None)),
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
    let user_ok = constant_time_eq(u.as_bytes(), user.as_bytes());
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    // Always run the argon2 verify, even when the username did not match, so a
    // wrong username costs the same time as a wrong password (no user oracle).
    let pw_ok = Argon2::default()
        .verify_password(p.as_bytes(), &parsed_hash)
        .is_ok();
    user_ok && pw_ok
}

// ── rate limiter ───────────────────────────────────────────────────────────

/// Check the rate limiter: max 10 failures per minute per IP.
pub fn check_rate_limit(auth: &AuthState, ip: IpAddr) -> bool {
    let mut guard = auth.rate_limiter.lock().unwrap();
    let entry = guard.entry(ip).or_default();
    let now = Instant::now();
    entry.retain(|t| now.duration_since(*t).as_secs() < 60);
    entry.len() < 10
}

/// Record a failed auth attempt.
pub fn rate_limit_record(auth: &AuthState, ip: IpAddr) {
    let mut guard = auth.rate_limiter.lock().unwrap();
    let entry = guard.entry(ip).or_default();
    entry.push(Instant::now());
    let now = Instant::now();
    entry.retain(|t| now.duration_since(*t).as_secs() < 60);
}

// ── client IP extraction ───────────────────────────────────────────────────

/// The client IP used for rate limiting.
///
/// The peer address of the TCP connection is the source of truth. A
/// `X-Forwarded-For` header is attacker-controlled, so it is honored *only*
/// when the peer itself is a loopback address — that is, when the request came
/// from a reverse proxy running on this same host. For any other peer the
/// header is ignored outright.
pub fn extract_client_ip(peer: IpAddr, headers: &HeaderMap) -> IpAddr {
    if !peer.is_loopback() {
        return peer;
    }
    if let Some(xff) = headers.get("x-forwarded-for")
        && let Ok(xff_str) = xff.to_str()
        && let Some(first) = xff_str.split(',').next().map(|s| s.trim())
        && let Ok(addr) = first.parse::<IpAddr>()
    {
        return addr;
    }
    peer
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

/// Verify a password against an argon2id PHC hash string (for the `users`
/// and `basic` auth modes — the low-level compare, not the HTTP header parse).
pub fn verify_basic_password(password: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ── session cookie (users mode) ────────────────────────────────────────────

use sha2::Sha256;

/// Cookie value: `base64(username || ":" || expiry || ":" || hmac(username:expiry, secret))`.
pub fn mint_session_cookie(username: &str, secret: &[u8]) -> String {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 7 * 24 * 3600; // 7 days
    let payload = format!("{username}:{expiry}");
    let mac = hmac_sha256_sign(&payload, secret);
    let cookie_val = format!("{payload}:{mac}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(cookie_val)
}

/// Verify a session cookie, returning the username if valid, or `None`.
pub fn verify_session_cookie(cookie_b64: &str, secret: &[u8]) -> Option<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cookie_b64)
        .ok()?;
    let cookie_val = std::str::from_utf8(&bytes).ok()?;

    let (payload, mac) = cookie_val.rsplit_once(':')?;
    let expected_mac = hmac_sha256_sign(payload, secret);
    if !constant_time_eq(mac.as_bytes(), expected_mac.as_bytes()) {
        return None;
    }

    let (username, expiry_str) = payload.split_once(':')?;
    let expiry: u64 = expiry_str.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if expiry < now {
        return None;
    }

    Some(username.to_string())
}

fn hmac_sha256_sign(msg: &str, key: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC key size");
    mac.update(msg.as_bytes());
    let result = mac.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(result.into_bytes())
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

    fn test_auth_state(mode: AuthMode) -> AuthState {
        let mut rng = rand::rng();
        let cookie_secret: [u8; 32] = rng.random();
        AuthState {
            mode,
            basic_user: None,
            basic_password_hash: None,
            token: None,
            cookie_secret: Arc::new(cookie_secret),
            users_db: Arc::new(Mutex::new(None)),
            rate_limiter: Arc::new(Mutex::new(HashMap::new())),
        }
    }

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
        let auth = test_auth_state(AuthMode::Token);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
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

    fn xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", value.parse().unwrap());
        headers
    }

    #[test]
    fn client_ip_direct_remote_peer_is_peer() {
        let peer: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(extract_client_ip(peer, &HeaderMap::new()), peer);
    }

    #[test]
    fn client_ip_loopback_peer_honors_forwarded_for() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let headers = xff("198.51.100.9, 10.0.0.1");
        assert_eq!(
            extract_client_ip(peer, &headers),
            "198.51.100.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn client_ip_loopback_peer_without_forwarded_for_is_loopback() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(extract_client_ip(peer, &HeaderMap::new()), peer);
    }

    #[test]
    fn client_ip_remote_peer_ignores_spoofed_forwarded_for() {
        let peer: IpAddr = "203.0.113.7".parse().unwrap();
        let headers = xff("1.2.3.4");
        assert_eq!(extract_client_ip(peer, &headers), peer);
    }

    #[test]
    fn client_ip_loopback_peer_ignores_garbage_forwarded_for() {
        let peer: IpAddr = "::1".parse().unwrap();
        let headers = xff("not-an-ip");
        assert_eq!(extract_client_ip(peer, &headers), peer);
    }

    #[test]
    fn basic_rejects_wrong_username_with_right_password() {
        let hash = hash_password("s3cret").unwrap();
        let mut headers = HeaderMap::new();
        let creds = base64::engine::general_purpose::STANDARD.encode("mallory:s3cret");
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {creds}").parse().unwrap(),
        );
        assert!(!verify_basic(&headers, Some("alice"), Some(&hash)));

        let creds = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {creds}").parse().unwrap(),
        );
        assert!(verify_basic(&headers, Some("alice"), Some(&hash)));
    }
}
