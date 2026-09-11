//! Cross-module tests: config -> keymap -> action -> layout, and real ptys
//! driven through the layout the way the app drives them.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::CommandBuilder;

use ttmux::action::{Action, Dir};
use ttmux::agent::{AgentState, Watcher};
use ttmux::config::Config;
use ttmux::input::{Keys, Resolution};
use ttmux::layout::{Layout, Mode, PaneId, Rect};
use ttmux::pty::Pane;

fn key(c: char, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), mods)
}

/// Drive a pane until `f` holds, or give up after a second.
fn pump_until(p: &mut Pane, f: impl Fn(&Pane) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        p.pump();
        if f(p) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn sh(id: PaneId, script: &str, cols: u16, rows: u16) -> Pane {
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.args(["-c", script]);
    Pane::spawn_cmd(id, cmd, 100, cols, rows).expect("spawn")
}

#[test]
fn default_keys_drive_the_default_layout() {
    // The exact path a keystroke takes in the app: config -> Keys -> Action.
    let cfg = Config::default();
    let mut keys = Keys::new(&cfg);
    assert_eq!(
        keys.resolve(key('a', KeyModifiers::CONTROL)),
        Resolution::Pending
    );
    let Resolution::Action(action) = keys.resolve(key('%', KeyModifiers::NONE)) else {
        panic!("prefix + % should split");
    };
    assert_eq!(action, Action::Split(Dir::Right));

    let mut layout = Layout::new(Rect::new(0, 0, 80, 24));
    layout.insert(1, None, None);
    let Action::Split(dir) = action else {
        unreachable!()
    };
    layout.insert(2, Some(1), Some(dir));

    let geo = layout.geometry();
    assert_eq!(geo.len(), 2);
    // A right split puts the new pane to the right of the old one.
    let left = geo.iter().find(|(id, _)| *id == 1).unwrap().1;
    let right = geo.iter().find(|(id, _)| *id == 2).unwrap().1;
    assert_eq!(left.x, 0);
    assert_eq!(right.x, left.right());
    assert_eq!(left.w + right.w, 80);
}

#[test]
fn every_default_binding_resolves_to_an_action() {
    let cfg = Config::default();
    let (map, errors) = cfg.keymap();
    assert!(errors.is_empty(), "{errors:?}");

    let mut keys = Keys::new(&cfg);
    for (binding, expected) in &map {
        let mut got = None;
        for (i, chord) in binding.0.iter().enumerate() {
            let ev = KeyEvent::new(chord.code, chord.mods);
            match keys.resolve(ev) {
                Resolution::Action(a) => got = Some(a),
                Resolution::Pending => assert_eq!(i, 0, "only the first chord may be a prefix"),
                Resolution::Passthrough => panic!("{binding} fell through at chord {i}"),
            }
        }
        assert_eq!(got.as_ref(), Some(expected), "{binding}");
    }
}

#[test]
fn panes_resize_to_their_tiles() {
    // Four panes in an 80x24 area, each pty sized to its tile's interior.
    let mut layout = Layout::new(Rect::new(0, 0, 80, 24));
    let mut panes = vec![];
    for id in 1..=4u32 {
        let near = (id > 1).then_some(id - 1);
        let dir = if id % 2 == 0 { Dir::Right } else { Dir::Down };
        layout.insert(id, near, Some(dir));
        panes.push(sh(id, "cat", 80, 24));
    }

    for (id, outer) in layout.geometry() {
        let inner = outer.shrink(1);
        let p = panes.iter_mut().find(|p| p.id == id).unwrap();
        p.resize(inner.w, inner.h);
        assert_eq!(p.screen().size(), (inner.h, inner.w));
    }

    for p in &mut panes {
        p.kill();
    }
}

#[test]
fn output_reaches_the_screen_and_the_agent_watcher() {
    let cfg = Config::default();
    let mut pane = sh(1, "printf 'Do you want to proceed? (y/n) '; sleep 5", 40, 6);
    assert!(
        pump_until(&mut pane, |p| p.tail(2).contains("(y/n)")),
        "prompt never appeared: {:?}",
        pane.tail(6)
    );

    let mut watcher = Watcher::new();
    let state = watcher.update(&cfg.agents, &pane.title(), &pane.tail(6), false);
    assert_eq!(state, AgentState::Attention);
    assert!(
        watcher.take_alert(),
        "first attention should raise an alert"
    );
    assert!(!watcher.take_alert(), "and only once");

    pane.kill();
}

#[test]
fn a_finished_pane_reports_dead_so_the_app_can_reap_it() {
    let mut pane = sh(1, "exit 7", 20, 5);
    assert!(pump_until(&mut pane, |p| p.is_dead()), "child never exited");
    assert_eq!(pane.exit_status(), Some(7));
}

#[test]
fn free_mode_keeps_geometry_then_lets_panes_move() {
    let mut layout = Layout::new(Rect::new(0, 0, 80, 24));
    layout.insert(1, None, None);
    layout.insert(2, Some(1), Some(Dir::Right));
    layout.insert(3, Some(2), Some(Dir::Down));
    let tiled: Vec<_> = layout.geometry();

    layout.set_mode(Mode::Free);
    assert_eq!(layout.geometry(), tiled, "switching to free must not jump");

    // Pane 3 sits in the bottom-right quarter, so it has room above it.
    let was = tiled.iter().find(|(id, _)| *id == 3).unwrap().1;
    layout.move_pane(3, Dir::Up, 3);
    let moved = layout.rect_of(3).unwrap();
    assert_eq!(moved.y, was.y - 3);
    assert_eq!(moved.x, was.x);

    // Panes stay inside the area however hard you shove them.
    layout.move_pane(3, Dir::Down, 500);
    assert!(layout.rect_of(3).unwrap().bottom() <= 24);
    layout.move_pane(3, Dir::Up, 500);
    assert_eq!(layout.rect_of(3).unwrap().y, 0);
}

#[test]
fn config_survives_a_save_load_round_trip_with_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ttmux.toml");

    let mut cfg = Config::default();
    cfg.general.scrollback = 500;
    cfg.status.left = vec!["session".into(), "host".into()];
    cfg.keys
        .insert("ctrl+a g".into(), "toggle-layout-mode".into());
    cfg.save(&path).unwrap();

    let loaded = Config::load(&path).unwrap();
    assert_eq!(loaded, cfg);

    // And the new binding actually works.
    let mut keys = Keys::new(&loaded);
    assert_eq!(
        keys.resolve(key('a', KeyModifiers::CONTROL)),
        Resolution::Pending
    );
    assert_eq!(
        keys.resolve(key('g', KeyModifiers::NONE)),
        Resolution::Action(Action::ToggleLayoutMode)
    );
}
