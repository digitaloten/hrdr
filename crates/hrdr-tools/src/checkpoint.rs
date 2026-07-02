//! File checkpoints: content-addressed pre-image snapshots so the agent's file
//! edits can be reverted per turn.
//!
//! Storage is git-like — each changed file's prior content is deflate-compressed
//! and stored once per unique content (content-addressed by SHA-256), and a
//! journal records which turn touched which file. Only files the agent modifies
//! (via `edit`/`write_file`) are snapshotted, and only their pre-image (the
//! content just before the first edit in a turn), so it's fast and small.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A single file change: the file `path` and its content hash *before* the turn
/// modified it (`pre = None` if the file didn't exist yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeRecord {
    turn: u64,
    ts: u64,
    path: String,
    pre: Option<String>,
}

/// One revertible checkpoint (a turn that changed files).
#[derive(Debug, Clone)]
pub struct CheckpointInfo {
    pub turn: u64,
    pub ts: u64,
    pub files: Vec<String>,
}

/// A disk-backed store of per-turn file pre-images.
pub struct Checkpoints {
    blobs_dir: PathBuf,
    journal_path: PathBuf,
    records: Vec<ChangeRecord>,
    turn: u64,
    touched: HashSet<String>,
}

impl Checkpoints {
    /// Open (or create) the checkpoint store rooted at `dir`.
    pub fn open(dir: PathBuf) -> Result<Self> {
        let blobs_dir = dir.join("blobs");
        std::fs::create_dir_all(&blobs_dir)
            .with_context(|| format!("creating {}", blobs_dir.display()))?;
        let journal_path = dir.join("journal.jsonl");
        let mut records = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&journal_path) {
            for line in text.lines() {
                if let Ok(r) = serde_json::from_str::<ChangeRecord>(line) {
                    records.push(r);
                }
            }
        }
        let turn = records.iter().map(|r| r.turn).max().unwrap_or(0);
        Ok(Self {
            blobs_dir,
            journal_path,
            records,
            turn,
            touched: HashSet::new(),
        })
    }

    /// Begin a new turn (its file changes form one checkpoint).
    pub fn begin_turn(&mut self) {
        self.turn += 1;
        self.touched.clear();
    }

    /// Record a file's pre-modification content (only on the first touch in the
    /// current turn). Call *before* a tool writes to `path`.
    pub fn record_pre(&mut self, path: &Path) {
        let key = path.to_string_lossy().to_string();
        if !self.touched.insert(key.clone()) {
            return; // already snapshotted this file this turn
        }
        let pre = match std::fs::read(path) {
            Ok(bytes) => match self.store_blob(&bytes) {
                Ok(hash) => Some(hash),
                Err(_) => return, // couldn't store — don't record a bad checkpoint
            },
            Err(_) => None, // file didn't exist before
        };
        let rec = ChangeRecord {
            turn: self.turn,
            ts: crate::unix_now(),
            path: key,
            pre,
        };
        if let Ok(line) = serde_json::to_string(&rec) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.journal_path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
        self.records.push(rec);
    }

    /// The revertible checkpoints (turns with changes), newest first.
    pub fn list(&self) -> Vec<CheckpointInfo> {
        let mut by_turn: BTreeMap<u64, CheckpointInfo> = BTreeMap::new();
        for r in &self.records {
            let e = by_turn.entry(r.turn).or_insert_with(|| CheckpointInfo {
                turn: r.turn,
                ts: r.ts,
                files: Vec::new(),
            });
            if !e.files.contains(&r.path) {
                e.files.push(r.path.clone());
            }
        }
        let mut v: Vec<_> = by_turn.into_values().collect();
        v.reverse();
        v
    }

    /// Revert the most recent turn's file changes. Returns the restored paths.
    pub fn revert_last(&mut self) -> Result<Vec<PathBuf>> {
        match self.records.iter().map(|r| r.turn).max() {
            Some(last) => self.revert_to(last),
            None => Ok(Vec::new()),
        }
    }

    /// Restore files to their state *before* `turn` — i.e. undo `turn` and every
    /// later turn. Returns the restored paths.
    pub fn revert_to(&mut self, turn: u64) -> Result<Vec<PathBuf>> {
        // For each file touched in turns >= `turn`, the pre-`turn` state is the
        // pre-image recorded at the SMALLEST such turn.
        let mut earliest: BTreeMap<String, (u64, Option<String>)> = BTreeMap::new();
        for r in self.records.iter().filter(|r| r.turn >= turn) {
            let e = earliest
                .entry(r.path.clone())
                .or_insert((r.turn, r.pre.clone()));
            if r.turn < e.0 {
                *e = (r.turn, r.pre.clone());
            }
        }
        let mut restored = Vec::new();
        for (path, (_t, pre)) in &earliest {
            let p = PathBuf::from(path);
            match pre {
                Some(hash) => {
                    let bytes = self.load_blob(hash)?;
                    if let Some(parent) = p.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&p, bytes)
                        .with_context(|| format!("restoring {}", p.display()))?;
                }
                None => {
                    let _ = std::fs::remove_file(&p); // didn't exist before the turn
                }
            }
            restored.push(p);
        }
        // Drop reverted records and rewrite the journal.
        self.records.retain(|r| r.turn < turn);
        self.rewrite_journal()?;
        Ok(restored)
    }

    fn store_blob(&self, bytes: &[u8]) -> Result<String> {
        let hash = sha256_hex(bytes);
        let path = self.blobs_dir.join(&hash);
        if !path.exists() {
            let compressed = miniz_oxide::deflate::compress_to_vec(bytes, 6);
            std::fs::write(&path, compressed).with_context(|| format!("writing blob {hash}"))?;
        }
        Ok(hash)
    }

    fn load_blob(&self, hash: &str) -> Result<Vec<u8>> {
        let comp = std::fs::read(self.blobs_dir.join(hash))
            .with_context(|| format!("reading blob {hash}"))?;
        miniz_oxide::inflate::decompress_to_vec(&comp)
            .map_err(|e| anyhow::anyhow!("decompressing blob {hash}: {e:?}"))
    }

    fn rewrite_journal(&self) -> Result<()> {
        let mut out = String::new();
        for r in &self.records {
            if let Ok(line) = serde_json::to_string(r) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        std::fs::write(&self.journal_path, out).context("rewriting checkpoint journal")?;
        Ok(())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revert_restores_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let existing = work.join("a.txt");
        std::fs::write(&existing, "original").unwrap();
        let created = work.join("b.txt");

        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        // Turn 1: modify a.txt, create b.txt.
        cp.begin_turn();
        cp.record_pre(&existing); // pre = "original"
        std::fs::write(&existing, "changed").unwrap();
        cp.record_pre(&created); // pre = None (new file)
        std::fs::write(&created, "new").unwrap();

        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "changed");
        assert!(created.exists());

        let restored = cp.revert_last().unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists(), "new file should be removed on revert");
        assert!(cp.list().is_empty(), "checkpoint consumed after revert");
    }

    #[test]
    fn revert_uses_earliest_preimage_across_turns() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn(); // turn 1
        cp.record_pre(&f); // pre = v0
        std::fs::write(&f, "v1").unwrap();

        cp.begin_turn(); // turn 2
        cp.record_pre(&f); // pre = v1
        std::fs::write(&f, "v2").unwrap();

        // Revert last (turn 2) → back to v1.
        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // Revert again (turn 1) → back to v0.
        cp.revert_last().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v0");
    }

    #[test]
    fn store_and_load_blob_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        let data = b"hello checkpoint world";
        let hash = cp.store_blob(data).unwrap();
        let loaded = cp.load_blob(&hash).unwrap();
        assert_eq!(loaded, data);
    }

    #[test]
    fn identical_blobs_are_deduped_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoints::open(dir.path().join("cp")).unwrap();
        let data = b"same content";
        let h1 = cp.store_blob(data).unwrap();
        let h2 = cp.store_blob(data).unwrap();
        // Same content → same hash, written only once.
        assert_eq!(h1, h2);
        let blob_count = std::fs::read_dir(&cp.blobs_dir).unwrap().count();
        assert_eq!(
            blob_count, 1,
            "identical content should produce exactly one blob file"
        );
    }

    #[test]
    fn record_pre_only_first_touch_recorded_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "original").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn();
        cp.record_pre(&f);
        cp.record_pre(&f); // second call for the same file in the same turn is a no-op
        // Only one journal record — the first touch.
        assert_eq!(cp.records.len(), 1);
    }

    #[test]
    fn revert_to_specific_turn_only_undoes_that_turn_forward() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut cp = Checkpoints::open(dir.path().join("cp")).unwrap();

        cp.begin_turn(); // turn 1: v0 → v1
        cp.record_pre(&f);
        std::fs::write(&f, "v1").unwrap();

        cp.begin_turn(); // turn 2: v1 → v2
        cp.record_pre(&f);
        std::fs::write(&f, "v2").unwrap();

        cp.begin_turn(); // turn 3: v2 → v3
        cp.record_pre(&f);
        std::fs::write(&f, "v3").unwrap();

        // revert_to(2) undoes turns 2 and 3; pre-turn-2 content is v1.
        cp.revert_to(2).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        // Turn 1 must still be listed — it was not reverted.
        let remaining = cp.list();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].turn, 1);
    }
}
