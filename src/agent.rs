//! Coding-agent activity detection: classify a pane as idle, busy, waiting on
//! the user, or done, from its title/output, so the status bar and borders
//! can flag the pane that needs attention.

use std::time::{Duration, Instant};

use crate::config;

/// How long a `Busy` pane is kept `Busy` after its output stops matching a
/// busy pattern, as long as the tail keeps changing (or just changed).
const GRACE: Duration = Duration::from_millis(1500);

/// Only the last few non-empty lines of a pane's tail are considered "recent"
/// enough to drive classification.
const RECENT_LINES: usize = 5;

/// How close to the end of the last line a `?`/`(y/n)`-style pattern must be
/// to count, so mid-prose punctuation doesn't false-positive.
const ANCHOR_CHARS: usize = 40;

/// Coarse activity state of a coding agent running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Busy,
    Attention,
    Done,
}

impl AgentState {
    /// Short unicode marker shown in borders/status bar.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Idle => "",
            AgentState::Busy => "◐",
            AgentState::Attention => "●",
            AgentState::Done => "✓",
        }
    }

    /// True only when the pane is actively waiting on the user.
    pub fn is_alert(self) -> bool {
        matches!(self, AgentState::Attention)
    }
}

impl Default for AgentState {
    fn default() -> Self {
        AgentState::Idle
    }
}

/// Per-pane classifier. Feed it on every pump via [`Watcher::update`].
pub struct Watcher {
    state: AgentState,
    /// Set on the transition into `Attention`; consumed by `take_alert`.
    alert_pending: bool,
    last_tail: String,
    last_change: Instant,
}

impl Watcher {
    pub fn new() -> Watcher {
        Watcher {
            state: AgentState::Idle,
            alert_pending: false,
            last_tail: String::new(),
            last_change: Instant::now(),
        }
    }

    /// Call on every pump. `bell` is a one-shot OS bell from the pane.
    pub fn update(&mut self, cfg: &config::Agents, title: &str, tail: &str, bell: bool) -> AgentState {
        self.update_at(cfg, title, tail, bell, Instant::now())
    }

    /// Same as `update`, but with an injectable clock for tests.
    fn update_at(
        &mut self,
        cfg: &config::Agents,
        title: &str,
        tail: &str,
        bell: bool,
        now: Instant,
    ) -> AgentState {
        if !cfg.enabled {
            self.set_state(AgentState::Idle);
            return self.state;
        }

        if tail != self.last_tail {
            self.last_tail = tail.to_string();
            self.last_change = now;
        }

        let lines = recent_lines(tail);

        let new_state = if bell {
            AgentState::Attention
        } else if matches_patterns(&cfg.attention_patterns, title, &lines) {
            AgentState::Attention
        } else if matches_patterns(&cfg.busy_patterns, title, &lines) {
            AgentState::Busy
        } else if matches_patterns(&cfg.done_patterns, title, &lines)
            && matches!(self.state, AgentState::Busy | AgentState::Attention)
        {
            AgentState::Done
        } else if self.state == AgentState::Busy && now.duration_since(self.last_change) < GRACE {
            AgentState::Busy
        } else {
            AgentState::Idle
        };

        self.set_state(new_state);
        new_state
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    /// True on the transition into `Attention` (the app rings the bell once).
    pub fn take_alert(&mut self) -> bool {
        std::mem::take(&mut self.alert_pending)
    }

    fn set_state(&mut self, new: AgentState) {
        if new == AgentState::Attention && self.state != AgentState::Attention {
            self.alert_pending = true;
        }
        self.state = new;
    }
}

impl Default for Watcher {
    fn default() -> Self {
        Watcher::new()
    }
}

/// The last few non-empty lines of `tail`, oldest first (so `.last()` is the
/// most recently written line).
fn recent_lines(tail: &str) -> Vec<&str> {
    let all: Vec<&str> = tail.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = all.len().saturating_sub(RECENT_LINES);
    all[start..].to_vec()
}

/// Patterns like `?` or `(y/n)` are ambiguous anywhere in old scrollback or
/// mid-sentence, so they only count near the end of the last line.
fn is_end_anchored(pattern: &str) -> bool {
    pattern.contains('?') || pattern.to_ascii_lowercase().contains("y/n")
}

fn last_n_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// Does `title` or the recent tail lines match any of `patterns`?
fn matches_patterns(patterns: &[String], title: &str, lines: &[&str]) -> bool {
    let last_line = lines.last().copied().unwrap_or("");
    for pat in patterns {
        if pat.is_empty() {
            continue;
        }
        if is_end_anchored(pat) {
            let end_title = last_n_chars(title, ANCHOR_CHARS);
            let end_line = last_n_chars(last_line, ANCHOR_CHARS);
            if contains_ci(&end_title, pat) || contains_ci(&end_line, pat) {
                return true;
            }
        } else {
            if contains_ci(title, pat) {
                return true;
            }
            if lines.iter().any(|l| contains_ci(l, pat)) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg() -> config::Agents {
        config::Agents::default()
    }

    #[test]
    fn disabled_is_always_idle() {
        let mut w = Watcher::new();
        let mut c = cfg();
        c.enabled = false;
        assert_eq!(
            w.update(&c, "claude: working", "esc to interrupt", true),
            AgentState::Idle
        );
    }

    #[test]
    fn bell_triggers_attention_alert_once() {
        let mut w = Watcher::new();
        assert_eq!(w.update(&cfg(), "", "", true), AgentState::Attention);
        assert!(w.take_alert());
        assert!(!w.take_alert());
    }

    #[test]
    fn esc_to_interrupt_is_busy() {
        let mut w = Watcher::new();
        assert_eq!(
            w.update(&cfg(), "", "Doing work...\nesc to interrupt", false),
            AgentState::Busy
        );
    }

    #[test]
    fn yes_no_prompt_is_attention() {
        let mut w = Watcher::new();
        assert_eq!(
            w.update(&cfg(), "", "Do you want to proceed? (y/n)", false),
            AgentState::Attention
        );
    }

    #[test]
    fn title_working_is_busy() {
        let mut w = Watcher::new();
        assert_eq!(
            w.update(&cfg(), "claude: working", "", false),
            AgentState::Busy
        );
    }

    #[test]
    fn done_only_counts_after_busy_or_attention() {
        let mut w = Watcher::new();
        assert_eq!(w.update(&cfg(), "", "done", false), AgentState::Idle);

        let mut w2 = Watcher::new();
        assert_eq!(
            w2.update(&cfg(), "", "thinking...", false),
            AgentState::Busy
        );
        assert_eq!(w2.update(&cfg(), "", "done", false), AgentState::Done);
    }

    #[test]
    fn old_attention_text_scrolls_out_of_range() {
        let mut w = Watcher::new();
        let mut tail = String::from("Continue? (y/n)\n");
        for i in 0..10 {
            tail.push_str(&format!("line {i}\n"));
        }
        assert_eq!(w.update(&cfg(), "", &tail, false), AgentState::Idle);
    }

    #[test]
    fn debounce_holds_busy_then_idles() {
        let mut w = Watcher::new();
        let t0 = Instant::now();
        assert_eq!(
            w.update_at(&cfg(), "", "thinking...", false, t0),
            AgentState::Busy
        );
        // Tail settles to something with no pattern match.
        let t1 = t0 + Duration::from_millis(10);
        assert_eq!(w.update_at(&cfg(), "", "$ ", false, t1), AgentState::Busy);
        // Still within the grace window, tail unchanged.
        let t2 = t1 + Duration::from_millis(1000);
        assert_eq!(w.update_at(&cfg(), "", "$ ", false, t2), AgentState::Busy);
        // Past the grace window.
        let t3 = t1 + Duration::from_millis(2000);
        assert_eq!(w.update_at(&cfg(), "", "$ ", false, t3), AgentState::Idle);
    }

    #[test]
    fn shell_prompt_is_not_attention() {
        let mut w = Watcher::new();
        assert_eq!(w.update(&cfg(), "", "~/dev/ttmux $ ", false), AgentState::Idle);
    }

    #[test]
    fn patterns_are_case_insensitive() {
        let mut w = Watcher::new();
        assert_eq!(
            w.update(&cfg(), "", "RUNNING build", false),
            AgentState::Busy
        );
    }

    #[test]
    fn custom_patterns_are_honoured() {
        let mut w = Watcher::new();
        let mut c = cfg();
        c.busy_patterns = vec!["frobnicating".into()];
        c.attention_patterns = vec!["gimme input".into()];
        assert_eq!(
            w.update(&c, "", "frobnicating the widgets", false),
            AgentState::Busy
        );
        let mut w2 = Watcher::new();
        assert_eq!(
            w2.update(&c, "", "gimme input please", false),
            AgentState::Attention
        );
    }

    #[test]
    fn glyphs_and_alert_flag() {
        assert_eq!(AgentState::Idle.glyph(), "");
        assert_eq!(AgentState::Busy.glyph(), "◐");
        assert_eq!(AgentState::Attention.glyph(), "●");
        assert_eq!(AgentState::Done.glyph(), "✓");
        assert!(AgentState::Attention.is_alert());
        assert!(!AgentState::Busy.is_alert());
        assert!(!AgentState::Done.is_alert());
        assert!(!AgentState::Idle.is_alert());
    }
}
