//! The pickers' shared state machine: a filterable, navigable list. Every
//! picker modal (`/model`, `/resume`, `/theme`, `/effort`, `/skills`, the
//! `/login` provider list) is a `Selector<T>` over its own choice type with a
//! fuzzy filter function; only what Enter *does* with the highlighted choice
//! differs, and that lives in each picker's key handler.

use hrdr_agent::{ModelChoice, filter_model_choices};
use hrdr_app::{
    EffortChoice, LoginProviderChoice, SessionMeta, Skill, ThemeChoice, filter_effort_choices,
    filter_login_providers, filter_sessions, filter_skills, filter_themes,
};

pub(crate) struct Selector<T> {
    /// All choices, in the order the picker's data source produced them.
    choices: Vec<T>,
    /// The fuzzy-find query typed into the picker's search line.
    pub(crate) filter: String,
    /// Indices into `choices` matching `filter`, in input order.
    filtered: Vec<usize>,
    /// Selected row within `filtered`.
    pub(crate) selected: usize,
    /// The picker's fuzzy filter (matching indices for a query).
    filter_fn: fn(&[T], &str) -> Vec<usize>,
}

impl<T> Selector<T> {
    pub(crate) fn new(choices: Vec<T>, filter_fn: fn(&[T], &str) -> Vec<usize>) -> Self {
        let filtered = (0..choices.len()).collect();
        Self {
            choices,
            filter: String::new(),
            filtered,
            selected: 0,
            filter_fn,
        }
    }

    fn refilter(&mut self) {
        self.filtered = (self.filter_fn)(&self.choices, &self.filter);
        self.selected = 0;
    }

    pub(crate) fn push_char(&mut self, c: char) {
        self.filter.push(c);
        self.refilter();
    }

    pub(crate) fn backspace(&mut self) {
        self.filter.pop();
        self.refilter();
    }

    pub(crate) fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// The filtered choices in display order.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &T> {
        self.filtered.iter().map(move |&i| &self.choices[i])
    }

    /// The currently-highlighted choice, if any survive the filter.
    pub(crate) fn current(&self) -> Option<&T> {
        self.filtered.get(self.selected).map(|&i| &self.choices[i])
    }
}

// ── Per-picker aliases + constructors (each pairs a choice type with its
//    fuzzy filter) ──────────────────────────────────────────────────────────

pub(crate) type ModelSelector = Selector<ModelChoice>;
pub(crate) fn model_selector(choices: Vec<ModelChoice>) -> ModelSelector {
    Selector::new(choices, filter_model_choices)
}

pub(crate) type SessionSelector = Selector<SessionMeta>;
pub(crate) fn session_selector(sessions: Vec<SessionMeta>) -> SessionSelector {
    Selector::new(sessions, filter_sessions)
}

pub(crate) type ThemeSelector = Selector<ThemeChoice>;
pub(crate) fn theme_selector(choices: Vec<ThemeChoice>) -> ThemeSelector {
    Selector::new(choices, filter_themes)
}

pub(crate) type EffortSelector = Selector<EffortChoice>;
pub(crate) fn effort_selector(choices: Vec<EffortChoice>) -> EffortSelector {
    Selector::new(choices, filter_effort_choices)
}

pub(crate) type SkillSelector = Selector<Skill>;
pub(crate) fn skill_selector(skills: Vec<Skill>) -> SkillSelector {
    Selector::new(skills, filter_skills)
}

pub(crate) type LoginProviderSelector = Selector<LoginProviderChoice>;
pub(crate) fn login_provider_selector(choices: Vec<LoginProviderChoice>) -> LoginProviderSelector {
    Selector::new(choices, filter_login_providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared machine: filter narrows (resetting the highlight), Up/Down
    /// clamp, an empty filter restores everything, and no match → no current.
    #[test]
    fn filter_navigate_and_select() {
        let choices =
            hrdr_app::choices_from(&["low".to_string(), "medium".to_string(), "high".to_string()]);
        let mut s = effort_selector(choices);
        assert_eq!(s.current().unwrap().label, "Default");
        s.up(); // clamps at the top
        assert_eq!(s.current().unwrap().label, "Default");
        s.down();
        assert_eq!(s.current().unwrap().label, "High");

        for c in "medium".chars() {
            s.push_char(c);
        }
        assert_eq!(s.rows().count(), 1);
        assert_eq!(s.current().unwrap().value.as_deref(), Some("medium"));

        s.push_char('z');
        assert!(s.current().is_none());
        s.backspace();
        assert_eq!(s.current().unwrap().label, "Medium");
    }
}
