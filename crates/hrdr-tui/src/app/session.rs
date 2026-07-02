//! Session persistence, restore, and transcript rebuild.

use hrdr_agent::{Message, Session};

use super::*;

impl super::App {
    /// On startup, resume the most recent saved session for the current
    /// directory (if any). No match → leave the fresh session as-is.
    pub(super) fn auto_resume_latest(&mut self) {
        let cwd = self.current_cwd();
        let cur = hrdr_agent::cwd_slug(&cwd);
        let Some(meta) = hrdr_agent::list_sessions()
            .into_iter()
            .find(|m| hrdr_agent::cwd_slug(&m.cwd) == cur)
        else {
            return; // nothing saved here yet — start fresh
        };
        let Ok(session) = Session::load_path(&meta.path) else {
            return;
        };
        // Skip empty sessions (system prompt only).
        if session.messages.len() <= 1 {
            return;
        }
        self.with_agent(|a| {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        });
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(meta.id);
        self.session_label = Some(session.name.clone());
        self.push_entry(Entry::System(format!(
            "resumed most recent session '{}' ({} messages) — /clear to start fresh",
            session.name,
            session.messages.len()
        )));
    }
    /// Persist the conversation. Sessions auto-save continuously: any non-empty
    /// conversation is written to disk, with a stable file id assigned (from the
    /// name) on first save. Called after every completed turn, `/undo`,
    /// `/retry`, and `/rename`.
    pub(super) fn autosave(&mut self) {
        let snap = self
            .agent
            .try_lock()
            .ok()
            .map(|a| (a.messages_owned(), a.cwd()));
        let Some((msgs, cwd)) = snap else {
            return;
        };
        let outcome = hrdr_app::save_session(
            self.session_id.as_deref(),
            self.session_label.as_deref(),
            &self.model,
            &self.base_url,
            &cwd.display().to_string(),
            msgs,
        );
        if let Some(o) = outcome {
            // Notify once, when the session is first created.
            if o.first_save {
                self.push_entry(Entry::System(format!(
                    "session saved as '{}' — /resume {}",
                    o.id, o.id
                )));
            }
            self.session_id = Some(o.id);
        }
    }
    /// Restore a resolved session (the shared `/resume` command calls this via
    /// [`hrdr_app::CommandHost::resume`]): swap in its messages/model, rebuild the
    /// transcript, adopt its id/name, and follow its working directory.
    pub(super) fn apply_session(&mut self, id: String, session: hrdr_agent::Session) {
        // A running turn holds the agent mutex: the message swap below would
        // silently no-op while the transcript + session id still switched, and
        // the in-flight turn's autosave would then overwrite the resumed
        // session's file with the old conversation.
        if self.running {
            self.system("a turn is running — interrupt it (Ctrl+C) before /resume");
            return;
        }
        let cwd = self.current_cwd();
        let count = session.messages.len();
        self.with_agent(|a| {
            a.set_messages(session.messages.clone());
            a.set_model(session.model.clone());
        });
        self.model = session.model.clone();
        self.rebuild_transcript(&session.messages);
        self.session_id = Some(id.clone());
        self.session_label = Some(session.name.clone());
        self.scroll_offset = 0;
        self.system(format!("resumed '{}' ({count} messages)", session.name));
        // Switch hrdr's tools to the session's working directory (in-process
        // only — the parent shell is untouched).
        if !session.cwd.is_empty() && session.cwd != cwd {
            let target = std::path::PathBuf::from(&session.cwd);
            if target.is_dir() {
                self.apply_cwd(target.clone());
                self.system(format!("cwd → {}", target.display()));
            } else {
                self.system(format!(
                    "note: session cwd {} no longer exists; staying in {cwd}",
                    session.cwd
                ));
            }
        }
        if session.base_url != self.base_url {
            self.system(format!(
                "note: session endpoint was {} (current: {})",
                session.base_url, self.base_url
            ));
        }
    }
    /// Rebuild the display transcript from a restored message history (the
    /// entry construction is shared with the GUI via
    /// [`hrdr_app::messages_to_entries`]).
    fn rebuild_transcript(&mut self, msgs: &[Message]) {
        self.clear_transcript();
        for e in hrdr_app::messages_to_entries(msgs) {
            self.push_entry(e);
        }
    }
}
