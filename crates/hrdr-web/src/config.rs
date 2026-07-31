//! Web-server configuration: the `[web]` table, CLI overrides, and the
//! refuse-to-bind matrix. Parsed from the same `config.toml` that feeds
//! `AgentConfig`, via `hrdr_agent::read_config_file`.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::Deserialize;

/// The `[web]` table from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebFileConfig {
    pub bind: String,
    pub port: u16,
    pub auth: String,
    pub basic_user: String,
    pub basic_password_hash: String,
    pub users_db: String,
    pub tls_cert_path: String,
    pub tls_key_path: String,
    pub allow_remote: bool,
}

impl Default for WebFileConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".into(),
            port: 9911,
            auth: "token".into(),
            basic_user: String::new(),
            basic_password_hash: String::new(),
            users_db: String::new(),
            tls_cert_path: String::new(),
            tls_key_path: String::new(),
            allow_remote: false,
        }
    }
}

/// The full `config.toml` root we deserialize — only the `[web]` table matters.
#[derive(Deserialize, Default)]
pub struct ConfigFile {
    pub web: Option<WebFileConfig>,
}

/// Resolved web-server configuration after file → env → CLI precedence.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub bind: IpAddr,
    pub port: u16,
    pub auth: AuthMode,
    pub basic_user: Option<String>,
    pub basic_password_hash: Option<String>,
    pub users_db: PathBuf,
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    pub allow_remote: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Token,
    Basic,
    Users,
}

impl AuthMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "token" => Some(Self::Token),
            "basic" => Some(Self::Basic),
            "users" => Some(Self::Users),
            _ => None,
        }
    }
}

/// CLI flags that override the config file and env vars.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub auth: Option<String>,
    pub allow_remote: bool,
    pub users_db: Option<PathBuf>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
}

impl WebConfig {
    /// Resolve the final configuration from file + env + CLI, returning any
    /// non-fatal warnings (e.g. unrecognised env values).
    pub fn load(cli: &CliOverrides) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();

        // 1. File defaults.
        let file: ConfigFile = hrdr_agent::read_config_file().unwrap_or_default();
        let web = file.web.unwrap_or_default();

        // 2. Env overrides.
        let bind_str = env_override("HRDR_WEB_BIND", &web.bind, &mut warnings);
        let port = env_override_u16("HRDR_WEB_PORT", web.port, &mut warnings);
        let auth_str = env_override("HRDR_WEB_AUTH", &web.auth, &mut warnings);
        let basic_user = env_override_opt("HRDR_WEB_BASIC_USER", web.basic_user);
        let basic_password_hash =
            env_override_opt("HRDR_WEB_BASIC_PASSWORD_HASH", web.basic_password_hash);
        let allow_remote =
            env_override_bool("HRDR_WEB_ALLOW_REMOTE", web.allow_remote, &mut warnings);
        let users_db = env_override_opt("HRDR_WEB_USERS_DB", web.users_db);
        let tls_cert = env_override_opt("HRDR_WEB_TLS_CERT", web.tls_cert_path);
        let tls_key = env_override_opt("HRDR_WEB_TLS_KEY", web.tls_key_path);

        // 3. CLI overrides (highest precedence).
        let bind_str = cli.bind.clone().unwrap_or(bind_str);
        let port = cli.port.unwrap_or(port);
        let auth_str = cli.auth.clone().unwrap_or(auth_str);
        let allow_remote = cli.allow_remote || allow_remote;
        let users_db_cli = cli.users_db.clone();
        let users_db = users_db_cli
            .or_else(|| {
                if users_db.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(&users_db))
                }
            })
            .unwrap_or_else(default_users_db);
        let tls_cert = cli.tls_cert.clone().or_else(|| {
            if tls_cert.is_empty() {
                None
            } else {
                Some(PathBuf::from(&tls_cert))
            }
        });
        let tls_key = cli.tls_key.clone().or_else(|| {
            if tls_key.is_empty() {
                None
            } else {
                Some(PathBuf::from(&tls_key))
            }
        });

        // Parse bind address.
        let bind: IpAddr = bind_str.parse().unwrap_or_else(|_| {
            warnings.push(format!(
                "invalid bind address '{bind_str}' — falling back to 127.0.0.1"
            ));
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))
        });

        // Parse auth mode.
        let auth = AuthMode::parse(&auth_str).unwrap_or_else(|| {
            warnings.push(format!(
                "unknown auth mode '{auth_str}' — falling back to token"
            ));
            AuthMode::Token
        });

        let basic_user = if basic_user.is_empty() {
            None
        } else {
            Some(basic_user)
        };
        let basic_password_hash = if basic_password_hash.is_empty() {
            None
        } else {
            Some(basic_password_hash)
        };

        let config = Self {
            bind,
            port,
            auth,
            basic_user,
            basic_password_hash,
            users_db,
            tls_cert_path: tls_cert,
            tls_key_path: tls_key,
            allow_remote,
        };

        (config, warnings)
    }
}

fn env_override(key: &str, default: &str, warnings: &mut Vec<String>) -> String {
    match std::env::var(key) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => default.to_string(),
        Err(e) => {
            warnings.push(format!("{key}: {e}"));
            default.to_string()
        }
    }
}

fn env_override_opt(key: &str, default: String) -> String {
    match std::env::var(key) {
        Ok(v) => v,
        Err(_) => default,
    }
}

fn env_override_u16(key: &str, default: u16, warnings: &mut Vec<String>) -> u16 {
    match std::env::var(key) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            warnings.push(format!("{key}: invalid port '{v}' — using default"));
            default
        }),
        Err(_) => default,
    }
}

fn env_override_bool(key: &str, default: bool, warnings: &mut Vec<String>) -> bool {
    match std::env::var(key) {
        Ok(v) => match v.to_lowercase().as_str() {
            "1" | "true" | "yes" => true,
            "0" | "false" | "no" => false,
            _ => {
                warnings.push(format!("{key}: expected true/false — ignoring"));
                default
            }
        },
        Err(_) => default,
    }
}

fn default_users_db() -> PathBuf {
    hrdr_agent::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("web")
        .join("users.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_loopback_token() {
        let (cfg, warnings) = WebConfig::load(&CliOverrides::default());
        assert!(cfg.bind.is_loopback());
        assert_eq!(cfg.port, 9911);
        assert_eq!(cfg.auth, AuthMode::Token);
        assert!(!cfg.allow_remote);
        assert!(warnings.is_empty());
    }

    #[test]
    fn cli_overrides_file() {
        let cli = CliOverrides {
            bind: Some("0.0.0.0".into()),
            port: Some(8080),
            auth: Some("basic".into()),
            allow_remote: true,
            ..Default::default()
        };
        let (cfg, _) = WebConfig::load(&cli);
        assert_eq!(cfg.bind.to_string(), "0.0.0.0");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.auth, AuthMode::Basic);
        assert!(cfg.allow_remote);
    }

    #[test]
    fn unknown_auth_warns() {
        let cli = CliOverrides {
            auth: Some("ldap".into()),
            ..Default::default()
        };
        let (cfg, warnings) = WebConfig::load(&cli);
        assert_eq!(cfg.auth, AuthMode::Token);
        assert!(!warnings.is_empty());
    }
}
