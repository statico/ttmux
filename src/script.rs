//! The scripting API: what `ttmux send-keys` and its siblings mean.
//!
//! This module only parses. The effects live in `App::script`, because a
//! command is the same thing a key binding is: a change to the running app.
//!
//! The names and short flags follow tmux, so a tmux script mostly reads the
//! same here. Every short flag also has a long spelling (`-t` is `--target`),
//! and `--json` on the listing commands prints machine-readable output, so an
//! agent never has to scrape a table.
//!
//! [`COMMANDS`] is the single source of truth. The CLI help, the in-app
//! command line, its completion and `API.md` all read from it, so a new
//! command documents itself by existing.

use std::fmt::Write as _;
use std::str::FromStr;

use anyhow::{bail, Result};
use crossterm::event::{KeyEvent, KeyModifiers};

use crate::action::{Action, Dir};
use crate::config::Chord;
use crate::layout::{PaneId, Preset};

/// What a finished command means to the shell that ran it. The `--help`
/// output is generated from here, so the table and the code cannot drift.
pub const EXIT_OK: u8 = 0;
/// Understood, but it could not be done: no such pane, no room to split.
pub const EXIT_ERROR: u8 = 1;
/// The arguments are wrong. Nothing ran.
pub const EXIT_USAGE: u8 = 2;
/// No session is listening.
pub const EXIT_NO_SESSION: u8 = 3;

pub const EXIT_CODES: &[(u8, &str)] = &[
    (EXIT_OK, "the command ran"),
    (EXIT_ERROR, "the command failed, for example no such pane"),
    (EXIT_USAGE, "the arguments are wrong, and nothing ran"),
    (EXIT_NO_SESSION, "no session is listening"),
];

// ------------------------------------------------------------------- specs

/// One flag of one command, for help and for completion.
pub struct Flag {
    pub short: Option<&'static str>,
    pub long: &'static str,
    /// The name of the value it takes, or `None` for an on/off flag.
    pub arg: Option<&'static str>,
    pub about: &'static str,
}

/// One scripting command as the documentation sees it.
pub struct Spec {
    pub name: &'static str,
    /// The heading it is listed under.
    pub group: &'static str,
    pub about: &'static str,
    /// Positional arguments in usage form: `KEY...`, `NAME`, or "".
    pub args: &'static str,
    pub flags: &'static [Flag],
    pub examples: &'static [&'static str],
}

impl Spec {
    /// The one-line usage: `send-keys [-t PANE] [-l] KEY...`.
    pub fn usage(&self) -> String {
        let mut out = self.name.to_string();
        for f in self.flags {
            let name = f.short.unwrap_or(f.long);
            match f.arg {
                Some(a) => {
                    let _ = write!(out, " [{name} {a}]");
                }
                None => {
                    let _ = write!(out, " [{name}]");
                }
            }
        }
        if !self.args.is_empty() {
            let _ = write!(out, " {}", self.args);
        }
        out
    }
}

const TARGET_PANE: Flag = Flag {
    short: Some("-t"),
    long: "--target",
    arg: Some("PANE"),
    about: "the pane to act on, as list-panes prints it; default the focused one",
};

const TARGET_WINDOW: Flag = Flag {
    short: Some("-t"),
    long: "--target",
    arg: Some("N"),
    about: "the window to act on, counted from 1; default the current one",
};

const JSON: Flag = Flag {
    short: None,
    long: "--json",
    arg: None,
    about: "print JSON instead of a table",
};

/// Every command. This order is the order the help prints.
pub const COMMANDS: &[Spec] = &[
    Spec {
        name: "send-keys",
        group: "panes",
        about: "type into a pane, as if the keys were pressed there",
        args: "KEY...",
        flags: &[
            TARGET_PANE,
            Flag {
                short: Some("-l"),
                long: "--literal",
                arg: None,
                about: "send the words as text, even when one is a key name",
            },
        ],
        examples: &[
            "ttmux send-keys 'make test' Enter",
            "ttmux send-keys --target %2 C-c",
            "ttmux send-keys -l Enter",
        ],
    },
    Spec {
        name: "capture-pane",
        group: "panes",
        about: "print what a pane shows, for reading a build or a test run",
        args: "",
        flags: &[
            TARGET_PANE,
            Flag {
                short: Some("-S"),
                long: "--history",
                arg: None,
                about: "include the scrollback, not only the visible screen",
            },
        ],
        examples: &["ttmux capture-pane", "ttmux capture-pane -t %2 --history"],
    },
    Spec {
        name: "split-window",
        group: "panes",
        about: "split a pane in two",
        args: "",
        flags: &[
            TARGET_PANE,
            Flag {
                short: Some("-h"),
                long: "--horizontal",
                arg: None,
                about: "side by side",
            },
            Flag {
                short: Some("-v"),
                long: "--vertical",
                arg: None,
                about: "one above the other, the default as in tmux",
            },
        ],
        examples: &["ttmux split-window -h", "ttmux split-window -t %1 -v"],
    },
    Spec {
        name: "select-pane",
        group: "panes",
        about: "focus a pane, by id or by direction",
        args: "",
        flags: &[
            TARGET_PANE,
            Flag {
                short: Some("-L"),
                long: "--left",
                arg: None,
                about: "focus the pane to the left, and -R, -U, -D for the rest",
            },
            Flag {
                short: None,
                long: "--next",
                arg: None,
                about: "focus the next pane in order, or --prev for the one before",
            },
        ],
        examples: &["ttmux select-pane -t %3", "ttmux select-pane -R"],
    },
    Spec {
        name: "resize-pane",
        group: "panes",
        about: "grow a pane, or zoom it to fill the window",
        args: "[N]",
        flags: &[
            TARGET_PANE,
            Flag {
                short: Some("-L"),
                long: "--left",
                arg: None,
                about: "the edge to push, and -R, -U, -D for the rest",
            },
            Flag {
                short: Some("-Z"),
                long: "--zoom",
                arg: None,
                about: "toggle zoom instead of resizing",
            },
        ],
        examples: &["ttmux resize-pane -R 10", "ttmux resize-pane -Z"],
    },
    Spec {
        name: "swap-pane",
        group: "panes",
        about: "exchange the places of two panes",
        args: "",
        flags: &[
            Flag {
                short: Some("-s"),
                long: "--source",
                arg: Some("PANE"),
                about: "the pane to move; default the focused one",
            },
            Flag {
                short: Some("-t"),
                long: "--target",
                arg: Some("PANE"),
                about: "the pane to swap it with",
            },
        ],
        examples: &["ttmux swap-pane -s %1 -t %2"],
    },
    Spec {
        name: "join-pane",
        group: "panes",
        about: "move a pane into another window",
        args: "",
        flags: &[
            Flag {
                short: Some("-s"),
                long: "--source",
                arg: Some("PANE"),
                about: "the pane to move; default the focused one",
            },
            Flag {
                short: Some("-t"),
                long: "--target",
                arg: Some("N"),
                about: "the window to move it into; default the current one",
            },
            Flag {
                short: Some("-h"),
                long: "--horizontal",
                arg: None,
                about: "place it side by side rather than below",
            },
        ],
        examples: &["ttmux join-pane -s %4 -t 1 -h"],
    },
    Spec {
        name: "break-pane",
        group: "panes",
        about: "move a pane into a new window of its own",
        args: "",
        flags: &[TARGET_PANE],
        examples: &["ttmux break-pane", "ttmux break-pane -t %2"],
    },
    Spec {
        name: "rename-pane",
        group: "panes",
        about: "name a pane; the name outlives any title the program sets",
        args: "NAME",
        flags: &[TARGET_PANE],
        examples: &["ttmux rename-pane build", "ttmux rename-pane -t %2 logs"],
    },
    Spec {
        name: "kill-pane",
        group: "panes",
        about: "close a pane",
        args: "",
        flags: &[TARGET_PANE],
        examples: &["ttmux kill-pane -t %3"],
    },
    Spec {
        name: "list-panes",
        group: "panes",
        about: "one line per pane",
        args: "",
        flags: &[
            Flag {
                short: Some("-a"),
                long: "--all",
                arg: None,
                about: "every window, not only the current one",
            },
            JSON,
        ],
        examples: &["ttmux list-panes", "ttmux list-panes -a --json"],
    },
    Spec {
        name: "new-window",
        group: "windows",
        about: "open a window",
        args: "",
        flags: &[Flag {
            short: Some("-n"),
            long: "--name",
            arg: Some("NAME"),
            about: "name it as it is created",
        }],
        examples: &["ttmux new-window -n logs"],
    },
    Spec {
        name: "select-window",
        group: "windows",
        about: "focus a window",
        args: "",
        flags: &[
            TARGET_WINDOW,
            Flag {
                short: Some("-n"),
                long: "--next",
                arg: None,
                about: "the next window, -p the one before, -l the last one",
            },
        ],
        examples: &["ttmux select-window -t 2", "ttmux select-window -n"],
    },
    Spec {
        name: "rename-window",
        group: "windows",
        about: "name a window",
        args: "NAME",
        flags: &[TARGET_WINDOW],
        examples: &["ttmux rename-window build"],
    },
    Spec {
        name: "swap-window",
        group: "windows",
        about: "exchange the places of two windows",
        args: "",
        flags: &[
            Flag {
                short: Some("-s"),
                long: "--source",
                arg: Some("N"),
                about: "the window to move; default the current one",
            },
            Flag {
                short: Some("-t"),
                long: "--target",
                arg: Some("N"),
                about: "the window to swap it with",
            },
        ],
        examples: &["ttmux swap-window -s 1 -t 3"],
    },
    Spec {
        name: "move-window",
        group: "windows",
        about: "renumber a window, sliding the others along",
        args: "",
        flags: &[
            Flag {
                short: Some("-s"),
                long: "--source",
                arg: Some("N"),
                about: "the window to move; default the current one",
            },
            Flag {
                short: Some("-t"),
                long: "--target",
                arg: Some("N"),
                about: "the position it takes",
            },
        ],
        examples: &["ttmux move-window -s 3 -t 1"],
    },
    Spec {
        name: "select-layout",
        group: "windows",
        about: "arrange the panes: even-horizontal, even-vertical, main-vertical, main-horizontal",
        args: "NAME",
        flags: &[],
        examples: &["ttmux select-layout main-vertical"],
    },
    Spec {
        name: "kill-window",
        group: "windows",
        about: "close a window and every pane in it",
        args: "",
        flags: &[TARGET_WINDOW],
        examples: &["ttmux kill-window -t 2"],
    },
    Spec {
        name: "list-windows",
        group: "windows",
        about: "one line per window",
        args: "",
        flags: &[JSON],
        examples: &["ttmux list-windows --json"],
    },
    Spec {
        name: "list-sessions",
        group: "session",
        about: "one line per session on this machine",
        args: "",
        flags: &[JSON],
        examples: &["ttmux list-sessions"],
    },
    Spec {
        name: "display-message",
        group: "session",
        about: "show text in the status bar",
        args: "TEXT",
        flags: &[],
        examples: &["ttmux display-message 'tests passed'"],
    },
    Spec {
        name: "show-options",
        group: "session",
        about: "print the config, or one dotted key of it",
        args: "[KEY]",
        flags: &[JSON],
        examples: &["ttmux show-options", "ttmux show-options appearance.gap"],
    },
    Spec {
        name: "set-option",
        group: "session",
        about: "write one dotted key into the config file, which reloads at once",
        args: "KEY VALUE",
        flags: &[],
        examples: &[
            "ttmux set-option appearance.border_style divider",
            "ttmux set-option appearance.gap 1",
        ],
    },
    Spec {
        name: "list-keys",
        group: "session",
        about: "every key binding and the action it runs",
        args: "",
        flags: &[JSON],
        examples: &["ttmux list-keys --json"],
    },
    Spec {
        name: "list-commands",
        group: "session",
        about: "every scripting command, its flags and its examples",
        args: "",
        flags: &[JSON],
        examples: &["ttmux list-commands --json"],
    },
    Spec {
        name: "run",
        group: "session",
        about: "run any key-binding action by name, such as toggle-zoom",
        args: "ACTION...",
        flags: &[],
        examples: &["ttmux run toggle-zoom", "ttmux run 'select-tab 2'"],
    },
];

pub fn spec(name: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// True for a verb this module handles, so the CLI can tell a scripting
/// command from a session command without parsing it twice.
pub fn is_command(verb: &str) -> bool {
    spec(verb).is_some() || alias(verb).is_some()
}

/// tmux spells some of these twice. The second spelling means the first,
/// sometimes with a flag already given.
fn alias(verb: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match verb {
        "move-pane" => ("join-pane", &[] as &[&str]),
        "next-window" => ("select-window", &["-n"]),
        "previous-window" | "prev-window" => ("select-window", &["-p"]),
        "last-window" => ("select-window", &["-l"]),
        "splitw" => ("split-window", &[]),
        "neww" => ("new-window", &[]),
        "killp" => ("kill-pane", &[]),
        "lsp" => ("list-panes", &[]),
        "lsw" => ("list-windows", &[]),
        "display" => ("display-message", &[]),
        _ => return None,
    })
}

/// The whole command list, grouped, for `ttmux --help`.
pub fn usage() -> String {
    let mut out = String::from("scripting commands:\n");
    let mut group = "";
    for c in COMMANDS {
        if c.group != group {
            group = c.group;
            let _ = write!(out, "\n  {group}\n");
        }
        let _ = writeln!(out, "    {:<36} {}", c.usage(), c.about);
    }
    out.push_str("\nEvery short flag has a long spelling: -t is --target.\n");
    out.push_str("`ttmux COMMAND --help` prints the flags and examples of one command.\n");
    out.push_str("\nexit codes:\n");
    for (code, why) in EXIT_CODES {
        let _ = writeln!(out, "    {code}  {why}");
    }
    out
}

/// Help for one command: what it does, its flags, and examples.
pub fn help(name: &str) -> Option<String> {
    let c = spec(name)?;
    let mut out = format!("{}\n\nusage: ttmux {}\n", c.about, c.usage());
    if !c.flags.is_empty() {
        out.push_str("\nflags:\n");
        for f in c.flags {
            let names = match f.short {
                Some(s) => format!("{s}, {}", f.long),
                None => format!("    {}", f.long),
            };
            let names = match f.arg {
                Some(a) => format!("{names} {a}"),
                None => names,
            };
            let _ = writeln!(out, "    {names:<26} {}", f.about);
        }
    }
    if !c.examples.is_empty() {
        out.push_str("\nexamples:\n");
        for e in c.examples {
            let _ = writeln!(out, "    {e}");
        }
    }
    Some(out)
}

/// Every command as JSON, so an agent can read the whole surface without
/// parsing help text.
pub fn commands_json() -> String {
    let cmds: Vec<serde_json::Value> = COMMANDS
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "group": c.group,
                "about": c.about,
                "usage": c.usage(),
                "args": c.args,
                "flags": c.flags.iter().map(|f| serde_json::json!({
                    "short": f.short,
                    "long": f.long,
                    "arg": f.arg,
                    "about": f.about,
                })).collect::<Vec<_>>(),
                "examples": c.examples,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "commands": cmds,
        "exit_codes": EXIT_CODES.iter()
            .map(|(c, why)| serde_json::json!({"code": c, "meaning": why}))
            .collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

// ----------------------------------------------------------------- parsing

/// One scripted command, already parsed and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Anything the keymap can do, named as a binding names it.
    Run(Action),
    SendKeys {
        target: Option<PaneId>,
        keys: Vec<KeyEvent>,
    },
    CapturePane {
        target: Option<PaneId>,
        history: bool,
    },
    Split {
        target: Option<PaneId>,
        dir: Dir,
    },
    SelectPane(PaneId),
    ResizePane {
        target: Option<PaneId>,
        dir: Dir,
        n: u16,
    },
    SwapPane {
        src: Option<PaneId>,
        dst: PaneId,
    },
    JoinPane {
        src: Option<PaneId>,
        window: Option<usize>,
        horizontal: bool,
    },
    BreakPane(Option<PaneId>),
    RenamePane {
        target: Option<PaneId>,
        name: String,
    },
    KillPane(Option<PaneId>),
    ListPanes {
        all: bool,
        json: bool,
    },
    NewWindow {
        name: Option<String>,
    },
    SelectWindow(usize),
    RenameWindow {
        target: Option<usize>,
        name: String,
    },
    SwapWindow {
        src: Option<usize>,
        dst: usize,
    },
    MoveWindow {
        src: Option<usize>,
        dst: usize,
    },
    KillWindow(Option<usize>),
    ListWindows {
        json: bool,
    },
    ListSessions {
        json: bool,
    },
    Display(String),
    ShowOptions {
        key: Option<String>,
        json: bool,
    },
    SetOption {
        key: String,
        value: String,
    },
    ListKeys {
        json: bool,
    },
    ListCommands {
        json: bool,
    },
}

/// Parse `verb` and its arguments. Errors name the command, because the
/// caller is a script and the message is all it gets.
pub fn parse(verb: &str, args: &[String]) -> Result<Cmd> {
    // An alias is the same command with some of its flags already given.
    if let Some((real, extra)) = alias(verb) {
        let mut all: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        all.extend(args.iter().cloned());
        return parse(real, &all);
    }
    let mut a = Args::new(args);
    let json = a.flag(&["--json"]);
    let cmd = match verb {
        "send-keys" => {
            let target = a.pane_target()?;
            let literal = a.flag(&["-l", "--literal"]);
            let words = a.rest();
            if words.is_empty() {
                bail!("send-keys needs at least one key");
            }
            let mut keys = vec![];
            for w in words {
                if literal {
                    keys.extend(chars(&w));
                } else {
                    keys.extend(key_word(&w));
                }
            }
            Cmd::SendKeys { target, keys }
        }
        "capture-pane" => {
            let target = a.pane_target()?;
            let history = a.flag(&["-S", "--history"]);
            a.end(verb)?;
            Cmd::CapturePane { target, history }
        }
        "split-window" => {
            // tmux's -h is "side by side" and -v is "stacked"; -v is the
            // default there, so it is the default here.
            let target = a.pane_target()?;
            let dir = if a.flag(&["-h", "--horizontal"]) {
                Dir::Right
            } else {
                a.flag(&["-v", "--vertical"]);
                Dir::Down
            };
            a.end(verb)?;
            Cmd::Split { target, dir }
        }
        "select-pane" => {
            let dir = a.direction();
            let next = a.flag(&["--next"]);
            let prev = a.flag(&["--prev", "--previous"]);
            let target = a.pane_target()?;
            a.end(verb)?;
            match (target, dir, next, prev) {
                (Some(_), Some(_), _, _) => {
                    bail!("select-pane takes a target or a direction, not both")
                }
                (Some(id), None, false, false) => Cmd::SelectPane(id),
                (None, Some(d), false, false) => Cmd::Run(Action::Focus(d)),
                (None, None, true, false) => Cmd::Run(Action::FocusNext),
                (None, None, false, true) => Cmd::Run(Action::FocusPrev),
                _ => bail!("select-pane needs -t PANE, a direction, --next or --prev"),
            }
        }
        "resize-pane" => {
            let target = a.pane_target()?;
            let zoom = a.flag(&["-Z", "--zoom"]);
            let dir = a.direction();
            let n = a.number()?;
            a.end(verb)?;
            match (zoom, dir) {
                (true, _) => Cmd::Run(Action::ToggleZoom),
                (false, Some(dir)) => Cmd::ResizePane {
                    target,
                    dir,
                    n: n.unwrap_or(2),
                },
                (false, None) => bail!("resize-pane needs a direction, or -Z"),
            }
        }
        "swap-pane" => {
            let src = a.pane_value(&["-s", "--source"])?;
            let dst = a.pane_target()?;
            a.end(verb)?;
            match dst {
                Some(dst) => Cmd::SwapPane { src, dst },
                None => bail!("swap-pane needs -t PANE"),
            }
        }
        "join-pane" => {
            let src = a.pane_value(&["-s", "--source"])?;
            let horizontal = a.flag(&["-h", "--horizontal"]);
            let window = a.window_target()?;
            a.end(verb)?;
            Cmd::JoinPane {
                src,
                window,
                horizontal,
            }
        }
        "break-pane" => {
            let target = a.pane_target()?;
            a.end(verb)?;
            Cmd::BreakPane(target)
        }
        "rename-pane" => {
            let target = a.pane_target()?;
            let name = a.joined();
            if name.is_empty() {
                bail!("rename-pane needs a name");
            }
            Cmd::RenamePane { target, name }
        }
        "kill-pane" => {
            let target = a.pane_target()?;
            a.end(verb)?;
            Cmd::KillPane(target)
        }
        "list-panes" => {
            let all = a.flag(&["-a", "--all"]);
            a.end(verb)?;
            Cmd::ListPanes { all, json }
        }
        "new-window" => {
            let name = a.value(&["-n", "--name"])?;
            a.end(verb)?;
            Cmd::NewWindow { name }
        }
        "select-window" => {
            if a.flag(&["-n", "--next"]) {
                a.end(verb)?;
                Cmd::Run(Action::NextTab)
            } else if a.flag(&["-p", "--prev", "--previous"]) {
                a.end(verb)?;
                Cmd::Run(Action::PrevTab)
            } else if a.flag(&["-l", "--last"]) {
                a.end(verb)?;
                Cmd::Run(Action::LastTab)
            } else {
                let target = a.window_target()?;
                a.end(verb)?;
                match target {
                    Some(n) => Cmd::SelectWindow(n),
                    None => bail!("select-window needs -t N, -n, -p or -l"),
                }
            }
        }
        "rename-window" => {
            let target = a.window_target()?;
            let name = a.joined();
            if name.is_empty() {
                bail!("rename-window needs a name");
            }
            Cmd::RenameWindow { target, name }
        }
        "swap-window" => {
            let src = a.window_value(&["-s", "--source"])?;
            let dst = a.window_target()?;
            a.end(verb)?;
            match dst {
                Some(dst) => Cmd::SwapWindow { src, dst },
                None => bail!("swap-window needs -t N"),
            }
        }
        "move-window" => {
            let src = a.window_value(&["-s", "--source"])?;
            let dst = a.window_target()?;
            a.end(verb)?;
            match dst {
                Some(dst) => Cmd::MoveWindow { src, dst },
                None => bail!("move-window needs -t N"),
            }
        }
        "select-layout" => {
            let name = a.joined();
            match Preset::from_str(&name) {
                Ok(p) => Cmd::Run(Action::SetPreset(p)),
                Err(e) => bail!("select-layout: {e}"),
            }
        }
        "kill-window" => {
            let target = a.window_target()?;
            a.end(verb)?;
            Cmd::KillWindow(target)
        }
        "list-windows" => {
            a.end(verb)?;
            Cmd::ListWindows { json }
        }
        "list-sessions" => {
            a.end(verb)?;
            Cmd::ListSessions { json }
        }
        "display-message" => {
            let text = a.joined();
            if text.is_empty() {
                bail!("display-message needs some text");
            }
            Cmd::Display(text)
        }
        "show-options" => {
            let key = a.joined();
            Cmd::ShowOptions {
                key: (!key.is_empty()).then_some(key),
                json,
            }
        }
        "set-option" => {
            let words = a.rest();
            let Some((key, value)) = words.split_first() else {
                bail!("set-option needs a key and a value");
            };
            if value.is_empty() {
                bail!("set-option needs a value for {key}");
            }
            Cmd::SetOption {
                key: key.clone(),
                value: value.join(" "),
            }
        }
        "list-keys" => {
            a.end(verb)?;
            Cmd::ListKeys { json }
        }
        "list-commands" => {
            a.end(verb)?;
            Cmd::ListCommands { json }
        }
        "run" => {
            let spec = a.joined();
            match Action::from_str(&spec) {
                Ok(action) => Cmd::Run(action),
                Err(e) => bail!("run: {e}"),
            }
        }
        other => bail!("unknown command {other}\n\n{}", usage()),
    };
    Ok(cmd)
}

/// Arguments with the flags picked out of them as they are asked for. Order
/// does not matter, which is how tmux behaves and what a script expects.
struct Args {
    words: Vec<String>,
}

impl Args {
    fn new(args: &[String]) -> Args {
        Args {
            words: args.to_vec(),
        }
    }

    /// True if any of `names` was present, removing it.
    fn flag(&mut self, names: &[&str]) -> bool {
        match self.words.iter().position(|w| names.contains(&w.as_str())) {
            Some(i) => {
                self.words.remove(i);
                true
            }
            None => false,
        }
    }

    /// The value of a flag, removed along with the flag. `--target=%2` is one
    /// word to the shell, and agents write it that way, so it is accepted too.
    fn value(&mut self, names: &[&str]) -> Result<Option<String>> {
        if let Some(i) = self
            .words
            .iter()
            .position(|w| names.iter().any(|n| w.starts_with(&format!("{n}="))))
        {
            let word = self.words.remove(i);
            let v = word.split_once('=').map(|(_, v)| v).unwrap_or_default();
            return Ok(Some(v.to_string()));
        }
        let Some(i) = self.words.iter().position(|w| names.contains(&w.as_str())) else {
            return Ok(None);
        };
        if i + 1 >= self.words.len() {
            bail!("{} needs a value", self.words[i]);
        }
        self.words.remove(i);
        Ok(Some(self.words.remove(i)))
    }

    /// `-t %3` or `-t 3`: a pane id, as `list-panes` prints it.
    fn pane_target(&mut self) -> Result<Option<PaneId>> {
        self.pane_value(&["-t", "--target"])
    }

    fn pane_value(&mut self, names: &[&str]) -> Result<Option<PaneId>> {
        let Some(v) = self.value(names)? else {
            return Ok(None);
        };
        let digits = v.strip_prefix('%').unwrap_or(&v);
        match digits.parse() {
            Ok(id) => Ok(Some(id)),
            Err(_) => bail!("not a pane id: {v}"),
        }
    }

    /// `-t 2`: a window, counted from 1 as the status bar counts them.
    fn window_target(&mut self) -> Result<Option<usize>> {
        self.window_value(&["-t", "--target"])
    }

    fn window_value(&mut self, names: &[&str]) -> Result<Option<usize>> {
        let Some(v) = self.value(names)? else {
            return Ok(None);
        };
        match v.parse() {
            Ok(n) if n >= 1 => Ok(Some(n)),
            _ => bail!("not a window number: {v}"),
        }
    }

    fn direction(&mut self) -> Option<Dir> {
        for (short, long, dir) in [
            ("-L", "--left", Dir::Left),
            ("-R", "--right", Dir::Right),
            ("-U", "--up", Dir::Up),
            ("-D", "--down", Dir::Down),
        ] {
            if self.flag(&[short, long]) {
                return Some(dir);
            }
        }
        None
    }

    /// A leading positional number, such as the cells in `resize-pane -R 10`.
    fn number(&mut self) -> Result<Option<u16>> {
        let Some(first) = self.words.first() else {
            return Ok(None);
        };
        if !first.chars().all(|c| c.is_ascii_digit()) {
            return Ok(None);
        }
        match self.words.remove(0).parse() {
            Ok(n) => Ok(Some(n)),
            Err(_) => bail!("not a number"),
        }
    }

    fn rest(self) -> Vec<String> {
        self.words
    }

    fn joined(self) -> String {
        self.words.join(" ")
    }

    /// Reject leftovers, so a misspelled flag is an error and not silence.
    fn end(&self, verb: &str) -> Result<()> {
        match self.words.first() {
            Some(w) => bail!("{verb}: unexpected argument {w}"),
            None => Ok(()),
        }
    }
}

/// One word of `send-keys`: a key name such as `Enter` or `C-c`, else the
/// word itself, character by character. tmux resolves it the same way.
fn key_word(word: &str) -> Vec<KeyEvent> {
    // tmux spells modifiers `C-x` and `M-x`; ttmux spells them `ctrl+x`.
    // Both are accepted, so a script can be pasted from either world.
    let spelled = match word.split_once('-') {
        Some((m, rest)) if matches!(m, "C" | "M" | "S") && !rest.is_empty() => {
            format!("{}+{rest}", m.to_ascii_lowercase())
        }
        _ => word.to_string(),
    };
    match Chord::from_str(&spelled) {
        // A single character parses as a chord too, and as a name it would
        // lose its case: "A" must stay a capital A.
        Ok(c) if c.mods.is_empty() && word.chars().count() == 1 => chars(word),
        Ok(c) => vec![KeyEvent::new(c.code, c.mods)],
        Err(_) => chars(word),
    }
}

fn chars(text: &str) -> Vec<KeyEvent> {
    text.chars()
        .map(|c| KeyEvent::new(crossterm::event::KeyCode::Char(c), KeyModifiers::NONE))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn parsed(line: &str) -> Cmd {
        let words = argv(line);
        parse(&words[0], &words[1..]).expect(line)
    }

    fn fails(line: &str) -> bool {
        let words = argv(line);
        parse(&words[0], &words[1..]).is_err()
    }

    #[test]
    fn send_keys_reads_names_modifiers_and_plain_text() {
        let Cmd::SendKeys { target, keys } = parsed("send-keys -t %2 hi Enter C-c") else {
            panic!("not send-keys");
        };
        assert_eq!(target, Some(2));
        assert_eq!(
            keys,
            vec![
                KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ]
        );
    }

    #[test]
    fn literal_send_keys_types_a_key_name_instead_of_pressing_it() {
        let Cmd::SendKeys { keys, .. } = parsed("send-keys -l Up") else {
            panic!("not send-keys");
        };
        let text: String = keys
            .iter()
            .filter_map(|k| match k.code {
                KeyCode::Char(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Up");
    }

    #[test]
    fn a_single_capital_letter_keeps_its_case() {
        let Cmd::SendKeys { keys, .. } = parsed("send-keys A") else {
            panic!("not send-keys");
        };
        assert_eq!(
            keys,
            vec![KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)]
        );
    }

    #[test]
    fn split_window_follows_tmux_and_stacks_by_default() {
        assert_eq!(
            parsed("split-window"),
            Cmd::Split {
                target: None,
                dir: Dir::Down
            }
        );
        assert_eq!(
            parsed("split-window -h"),
            Cmd::Split {
                target: None,
                dir: Dir::Right
            }
        );
    }

    #[test]
    fn every_short_flag_has_a_long_spelling_that_means_the_same() {
        assert_eq!(
            parsed("send-keys -t %2 x"),
            parsed("send-keys --target %2 x")
        );
        assert_eq!(
            parsed("send-keys --target=%2 x"),
            parsed("send-keys -t 2 x")
        );
        assert_eq!(
            parsed("split-window -h"),
            parsed("split-window --horizontal")
        );
        assert_eq!(parsed("list-panes -a"), parsed("list-panes --all"));
    }

    #[test]
    fn select_pane_takes_a_target_or_a_direction() {
        assert_eq!(parsed("select-pane -t 3"), Cmd::SelectPane(3));
        assert_eq!(parsed("select-pane -L"), Cmd::Run(Action::Focus(Dir::Left)));
        assert_eq!(parsed("select-pane --next"), Cmd::Run(Action::FocusNext));
        assert!(fails("select-pane -t 3 -L"));
        assert!(fails("select-pane"));
    }

    #[test]
    fn resize_pane_takes_an_amount_and_zooms_with_capital_z() {
        assert_eq!(
            parsed("resize-pane -R 10"),
            Cmd::ResizePane {
                target: None,
                dir: Dir::Right,
                n: 10
            }
        );
        assert_eq!(parsed("resize-pane -Z"), Cmd::Run(Action::ToggleZoom));
        assert!(fails("resize-pane 4"));
    }

    #[test]
    fn panes_move_between_windows() {
        assert_eq!(
            parsed("join-pane -s %4 -t 2 -h"),
            Cmd::JoinPane {
                src: Some(4),
                window: Some(2),
                horizontal: true
            }
        );
        // tmux's other name for it.
        assert_eq!(
            parsed("move-pane -s %4 -t 2"),
            parsed("join-pane -s %4 -t 2")
        );
        assert_eq!(parsed("break-pane"), Cmd::BreakPane(None));
        assert_eq!(
            parsed("swap-pane -s %1 -t %2"),
            Cmd::SwapPane {
                src: Some(1),
                dst: 2
            }
        );
        assert!(fails("swap-pane -s %1"));
    }

    #[test]
    fn windows_swap_and_renumber() {
        assert_eq!(
            parsed("swap-window -s 1 -t 3"),
            Cmd::SwapWindow {
                src: Some(1),
                dst: 3
            }
        );
        assert_eq!(
            parsed("move-window -t 1"),
            Cmd::MoveWindow { src: None, dst: 1 }
        );
        assert!(fails("move-window -s 2"));
    }

    #[test]
    fn the_tmux_aliases_mean_their_long_names() {
        assert_eq!(parsed("next-window"), Cmd::Run(Action::NextTab));
        assert_eq!(parsed("last-window"), Cmd::Run(Action::LastTab));
        assert_eq!(parsed("lsw"), Cmd::ListWindows { json: false });
        assert_eq!(parsed("neww -n logs"), parsed("new-window --name logs"));
    }

    #[test]
    fn run_takes_any_action_the_keymap_takes() {
        assert_eq!(parsed("run toggle-zoom"), Cmd::Run(Action::ToggleZoom));
        assert_eq!(
            parsed("run split right"),
            Cmd::Run(Action::Split(Dir::Right))
        );
        assert!(fails("run not-an-action"));
    }

    #[test]
    fn select_layout_names_a_preset() {
        assert_eq!(
            parsed("select-layout main-vertical"),
            Cmd::Run(Action::SetPreset(Preset::MainVertical))
        );
        assert!(fails("select-layout sideways"));
    }

    #[test]
    fn set_option_keeps_the_key_and_the_rest_as_the_value() {
        assert_eq!(
            parsed("set-option appearance.border_style divider"),
            Cmd::SetOption {
                key: "appearance.border_style".into(),
                value: "divider".into(),
            }
        );
        assert!(fails("set-option appearance.gap"));
    }

    #[test]
    fn a_name_with_spaces_survives_the_shell_splitting_it() {
        assert_eq!(
            parsed("rename-window -t 2 build and test"),
            Cmd::RenameWindow {
                target: Some(2),
                name: "build and test".into(),
            }
        );
    }

    #[test]
    fn a_misspelled_flag_is_an_error_rather_than_silence() {
        assert!(fails("list-panes -x"));
        assert!(fails("kill-pane -t %1 oops"));
    }

    #[test]
    fn a_bad_target_is_an_error_rather_than_pane_zero() {
        assert!(fails("kill-pane -t nope"));
        assert!(fails("select-window -t 0"));
    }

    #[test]
    fn every_command_parses_its_own_examples() {
        for c in COMMANDS {
            for ex in c.examples {
                let words = shell_words(ex);
                assert_eq!(words[0], "ttmux", "{ex}");
                parse(&words[1], &words[2..]).unwrap_or_else(|e| panic!("{ex}: {e:#}"));
            }
        }
    }

    /// Split on spaces, keeping '...' together, the way a shell would.
    fn shell_words(line: &str) -> Vec<String> {
        let mut out = vec![];
        let mut cur = String::new();
        let mut quoted = false;
        for c in line.chars() {
            match c {
                '\'' => quoted = !quoted,
                ' ' if !quoted => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                _ => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn every_command_has_help_and_appears_in_the_usage() {
        let all = usage();
        for c in COMMANDS {
            assert!(all.contains(c.name), "{} missing from usage", c.name);
            let h = help(c.name).unwrap_or_else(|| panic!("no help for {}", c.name));
            assert!(h.contains(c.about));
            assert!(is_command(c.name));
        }
        assert!(help("nope").is_none());
    }

    #[test]
    fn the_command_list_is_machine_readable() {
        let doc: serde_json::Value = serde_json::from_str(&commands_json()).unwrap();
        let names: Vec<&str> = doc["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"send-keys"));
        assert_eq!(names.len(), COMMANDS.len());
        assert_eq!(doc["exit_codes"][0]["code"], 0);
    }
}
