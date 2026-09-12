//! Custom status widgets: a shell command whose output goes in the bar.
//!
//! Commands run on their own threads and the bar reads the last line each of
//! them printed, so a slow script never stalls a frame. A widget that is
//! still running when its next turn comes round is left alone: it falls
//! behind rather than piling up.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::CustomWidget;

/// How long a command gets before it is treated as hung. It keeps running --
/// killing a process group here would be worse than a stale widget -- but its
/// slot is freed so later runs are not blocked behind it forever.
const HUNG: Duration = Duration::from_secs(30);

struct Slot {
    cfg: CustomWidget,
    out: Arc<Mutex<String>>,
    running: Arc<AtomicBool>,
    /// When the last run started. `None` until the widget has run at all.
    started: Option<Instant>,
}

#[derive(Default)]
pub struct Runner {
    slots: BTreeMap<String, Slot>,
}

impl Runner {
    /// Match the runner to the config. A widget whose definition is unchanged
    /// keeps its output, so editing one line of the config does not blank
    /// every other widget in the bar.
    pub fn reload(&mut self, cfg: &BTreeMap<String, CustomWidget>) {
        self.slots.retain(|name, s| cfg.get(name) == Some(&s.cfg));
        for (name, w) in cfg {
            self.slots.entry(name.clone()).or_insert_with(|| Slot {
                cfg: w.clone(),
                out: Arc::new(Mutex::new(String::new())),
                running: Arc::new(AtomicBool::new(false)),
                started: None,
            });
        }
    }

    /// Start whatever is due. Cheap enough to call every frame.
    pub fn tick(&mut self) {
        for slot in self.slots.values_mut() {
            if !slot.due() {
                continue;
            }
            // `swap` rather than a load and a store: two ticks in the same
            // millisecond would otherwise both see it idle and both spawn.
            if slot.running.swap(true, Ordering::SeqCst) {
                continue;
            }
            slot.started = Some(Instant::now());
            let cmd = slot.cfg.command.clone();
            let out = slot.out.clone();
            let running = slot.running.clone();
            std::thread::spawn(move || {
                let text = run(&cmd);
                *out.lock().unwrap() = text;
                running.store(false, Ordering::SeqCst);
            });
        }
    }

    /// The last thing `name` printed, or `None` if there is no such widget.
    pub fn output(&self, name: &str) -> Option<String> {
        self.slots.get(name).map(|s| s.out.lock().unwrap().clone())
    }
}

impl Slot {
    fn due(&self) -> bool {
        let Some(started) = self.started else {
            return true;
        };
        if self.running.load(Ordering::SeqCst) {
            return started.elapsed() >= HUNG;
        }
        // Zero is "once": having run at all is enough.
        self.cfg.interval > 0 && started.elapsed() >= Duration::from_secs(self.cfg.interval)
    }
}

/// Run one command and collapse its output to a single line.
///
/// stderr is dropped rather than shown: a script that warns on every run
/// would otherwise make the bar unreadable, and there is nowhere to scroll.
fn run(cmd: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines()
                .map(str::trim_end)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
        // Visible rather than blank: a widget that silently shows nothing
        // looks the same as one that is working and has nothing to say.
        Err(e) => format!("!{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(command: &str, interval: u64) -> BTreeMap<String, CustomWidget> {
        BTreeMap::from([(
            "w".to_string(),
            CustomWidget {
                command: command.into(),
                interval,
            },
        )])
    }

    /// Waits for the widget's thread rather than sleeping a fixed amount.
    fn settle(r: &mut Runner) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            r.tick();
            let out = r.output("w").unwrap();
            if !out.is_empty() || Instant::now() > deadline {
                return out;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_widget_shows_what_its_command_printed() {
        let mut r = Runner::default();
        r.reload(&cfg("echo hello", 60));
        assert_eq!(settle(&mut r), "hello");
    }

    #[test]
    fn the_command_goes_through_a_shell_so_pipes_work() {
        let mut r = Runner::default();
        r.reload(&cfg("echo a b c | tr ' ' -", 60));
        assert_eq!(settle(&mut r), "a-b-c");
    }

    #[test]
    fn several_printed_lines_collapse_into_one() {
        let mut r = Runner::default();
        r.reload(&cfg("printf 'one\\n\\ntwo\\n'", 60));
        assert_eq!(settle(&mut r), "one two");
    }

    #[test]
    fn stderr_stays_out_of_the_bar() {
        let mut r = Runner::default();
        r.reload(&cfg("echo out; echo err >&2", 60));
        assert_eq!(settle(&mut r), "out");
    }

    #[test]
    fn an_unchanged_widget_keeps_its_output_across_a_reload() {
        let mut r = Runner::default();
        r.reload(&cfg("echo hello", 60));
        assert_eq!(settle(&mut r), "hello");
        r.reload(&cfg("echo hello", 60));
        assert_eq!(r.output("w").unwrap(), "hello");
    }

    #[test]
    fn an_edited_widget_starts_over_rather_than_showing_the_old_output() {
        let mut r = Runner::default();
        r.reload(&cfg("echo hello", 60));
        assert_eq!(settle(&mut r), "hello");
        r.reload(&cfg("echo goodbye", 60));
        assert_eq!(r.output("w").unwrap(), "");
        assert_eq!(settle(&mut r), "goodbye");
    }

    #[test]
    fn a_widget_dropped_from_the_config_is_gone() {
        let mut r = Runner::default();
        r.reload(&cfg("echo hello", 60));
        r.reload(&BTreeMap::new());
        assert!(r.output("w").is_none());
    }

    #[test]
    fn an_interval_of_zero_runs_once_and_then_stops() {
        let mut r = Runner::default();
        r.reload(&cfg("date +%s%N", 0));
        let first = settle(&mut r);
        assert!(!first.is_empty());
        for _ in 0..20 {
            r.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(r.output("w").unwrap(), first);
    }

    #[test]
    fn a_widget_runs_again_once_its_interval_has_passed() {
        let mut r = Runner::default();
        r.reload(&cfg("date +%s%N", 1));
        let first = settle(&mut r);
        let deadline = Instant::now() + Duration::from_secs(10);
        while r.output("w").unwrap() == first && Instant::now() < deadline {
            r.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(r.output("w").unwrap(), first, "the widget never re-ran");
    }

    #[test]
    fn a_slow_command_does_not_start_a_second_copy_of_itself() {
        let mut r = Runner::default();
        // Appends on every run, so a pile-up shows up as extra output.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("runs");
        let cmd = format!("printf x >> {0}; sleep 1; cat {0}", f.display());
        r.reload(&cfg(&cmd, 0));
        for _ in 0..50 {
            r.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x");
    }
}
