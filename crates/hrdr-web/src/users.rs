//! SQLite user store for the `users` auth mode. Schema:
//! `users(username TEXT PRIMARY KEY, password_hash TEXT NOT NULL, created INTEGER NOT NULL)`.
//! Password verification uses argon2id via `auth::verify_basic`.

use std::path::Path;

use anyhow::Context;
use rusqlite::Connection;

/// Open (or create) the users database at `path`.
pub fn open_db(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dirs for {}", path.display()))?;
    }
    let db = Connection::open(path)
        .with_context(|| format!("opening users db at {}", path.display()))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
            username TEXT PRIMARY KEY,
            password_hash TEXT NOT NULL,
            created INTEGER NOT NULL
        )",
    )?;
    Ok(db)
}

/// Add a user with an argon2id password hash.
pub fn add_user(db: &Connection, username: &str, password_hash: &str) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    db.execute(
        "INSERT OR REPLACE INTO users (username, password_hash, created) VALUES (?1, ?2, ?3)",
        rusqlite::params![username, password_hash, now],
    )?;
    Ok(())
}

/// Remove a user.
pub fn remove_user(db: &Connection, username: &str) -> anyhow::Result<bool> {
    let n = db.execute(
        "DELETE FROM users WHERE username = ?1",
        rusqlite::params![username],
    )?;
    Ok(n > 0)
}

/// Look up a user's password hash. Returns `None` if the user does not exist.
pub fn get_password_hash(db: &Connection, username: &str) -> anyhow::Result<Option<String>> {
    let mut stmt = db.prepare("SELECT password_hash FROM users WHERE username = ?1")?;
    let hash: Option<String> = stmt
        .query_row(rusqlite::params![username], |row| row.get(0))
        .optional()?;
    Ok(hash)
}

/// Verify a username + password against the database.
pub fn verify(db: &Connection, username: &str, password: &str) -> anyhow::Result<bool> {
    let Some(hash) = get_password_hash(db, username)? else {
        return Ok(false);
    };
    Ok(crate::auth::verify_basic_password(password, &hash))
}

// Allow `optional()` on rusqlite rows.
trait OptionalExt<T> {
    fn optional(self) -> rusqlite::Result<Option<T>>;
}

impl<T> OptionalExt<T> for rusqlite::Result<T> {
    fn optional(self) -> rusqlite::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::hash_password;

    #[test]
    fn add_then_verify_and_remove_user() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
                username TEXT PRIMARY KEY,
                password_hash TEXT NOT NULL,
                created INTEGER NOT NULL
            )",
        )
        .unwrap();

        let hash = hash_password("mypass").unwrap();

        // Add.
        add_user(&db, "alice", &hash).unwrap();
        assert!(get_password_hash(&db, "alice").unwrap().is_some());

        // Verify.
        assert!(verify(&db, "alice", "mypass").unwrap());
        assert!(!verify(&db, "alice", "wrong").unwrap());
        assert!(!verify(&db, "bob", "mypass").unwrap());

        // Remove.
        assert!(remove_user(&db, "alice").unwrap());
        assert!(get_password_hash(&db, "alice").unwrap().is_none());
        assert!(!remove_user(&db, "alice").unwrap());
    }
}
