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
    arg: Some("WINDOW"),
    about: "the window to act on, by number from 1 or by name; default the current one",
};

const JSON: Flag = Flag {
    short: None,
    long: "--json",
    arg: None,
    about: "print JSON instead of a table",
};

const FORMAT: Flag = Flag {
    short: Some("-F"),
    long: "--format",
    arg: Some("FORMAT"),
    about: "print one line per row, filling #{field} from the JSON, as in tmux",
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
                long: "--start",
                arg: Some("[LINE]"),
                about: "include scrollback: - for all of it, -50 for the 50 lines above the screen",
            },
            Flag {
                short: Some("-p"),
                long: "--print",
                arg: None,
                about: "print to stdout, which capture-pane always does; accepted for tmux scripts",
            },
        ],
        examples: &[
            "ttmux capture-pane",
            "ttmux capture-pane -t %2 -S -",
            "ttmux capture-pane -p -S -50",
        ],
    },
    Spec {
        name: "split-window",
        group: "panes",
        about: "split a pane in two, running COMMAND or a shell in the new one",
        args: "[COMMAND]",
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
        examples: &[
            "ttmux split-window -h",
            "ttmux split-window -t %1 -v",
            "ttmux split-window -h 'tail -f log/dev.log'",
        ],
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
                arg: Some("WINDOW"),
                about: "the window to move it into, by number or name; default the current one",
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
            FORMAT,
        ],
        examples: &[
            "ttmux list-panes",
            "ttmux list-panes -a --json",
            "ttmux list-panes -F '#{pane_id} #{pane_title}'",
        ],
    },
    Spec {
        name: "new-window",
        group: "windows",
        about: "open a window, running COMMAND or a shell in it",
        args: "[COMMAND]",
        flags: &[Flag {
            short: Some("-n"),
            long: "--name",
            arg: Some("NAME"),
            about: "name it as it is created",
        }],
        examples: &["ttmux new-window -n logs", "ttmux new-window -n top htop"],
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
                arg: Some("WINDOW"),
                about: "the window to move, by number or name; default the current one",
            },
            Flag {
                short: Some("-t"),
                long: "--target",
                arg: Some("WINDOW"),
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
                arg: Some("WINDOW"),
                about: "the window to move, by number or name; default the current one",
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
        flags: &[JSON, FORMAT],
        examples: &[
            "ttmux list-windows --json",
            "ttmux list-windows -F '#{window_index}:#{window_name}'",
        ],
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
        name: "doctor",
        group: "session",
        about: "report why panes here cannot reach the keychain or a permission prompt",
        args: "",
        flags: &[],
        examples: &["ttmux doctor"],
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

/// tmux spells most commands twice, and some three times. The other
/// spellings mean the real name, sometimes with a flag already given.
/// `(alias, command, flags)`.
pub const ALIASES: &[(&str, &str, &[&str])] = &[
    ("send", "send-keys", &[]),
    ("capturep", "capture-pane", &[]),
    ("splitw", "split-window", &[]),
    ("selectp", "select-pane", &[]),
    ("resizep", "resize-pane", &[]),
    ("swapp", "swap-pane", &[]),
    ("move-pane", "join-pane", &[]),
    ("joinp", "join-pane", &[]),
    ("breakp", "break-pane", &[]),
    ("killp", "kill-pane", &[]),
    ("lsp", "list-panes", &[]),
    ("neww", "new-window", &[]),
    ("selectw", "select-window", &[]),
    ("next-window", "select-window", &["-n"]),
    ("next", "select-window", &["-n"]),
    ("previous-window", "select-window", &["-p"]),
    ("prev-window", "select-window", &["-p"]),
    ("prev", "select-window", &["-p"]),
    ("last-window", "select-window", &["-l"]),
    ("last", "select-window", &["-l"]),
    ("renamew", "rename-window", &[]),
    ("swapw", "swap-window", &[]),
    ("movew", "move-window", &[]),
    ("selectl", "select-layout", &[]),
    ("killw", "kill-window", &[]),
    ("lsw", "list-windows", &[]),
    ("ls", "list-sessions", &[]),
    ("display", "display-message", &[]),
    ("show", "show-options", &[]),
    ("set", "set-option", &[]),
    ("lsk", "list-keys", &[]),
];

fn alias(verb: &str) -> Option<(&'static str, &'static [&'static str])> {
    ALIASES
        .iter()
        .find(|(a, _, _)| *a == verb)
        .map(|(_, real, flags)| (*real, *flags))
}

/// The other spellings of a command, for its help.
fn aliases_of(name: &str) -> Vec<&'static str> {
    ALIASES
        .iter()
        .filter(|(_, real, flags)| *real == name && flags.is_empty())
        .map(|(a, _, _)| *a)
        .collect()
}

/// The real command a verb names, alias or not. `None` for a word that is
/// no command at all.
pub fn spec_name(verb: &str) -> Option<&'static str> {
    let real = alias(verb).map_or(verb, |(real, _)| real);
    spec(real).map(|s| s.name)
}

/// Whether a command offers `--json`. The CLI asks too, so `-h` and
/// `--json` mean the same thing in both places.
pub fn takes_json(verb: &str) -> bool {
    has_flag(verb, "--json")
}

/// Whether a command declares a flag by either of its spellings.
pub fn has_flag(verb: &str, name: &str) -> bool {
    let real = alias(verb).map_or(verb, |(real, _)| real);
    spec(real).is_some_and(|s| {
        s.flags
            .iter()
            .any(|f| f.long == name || f.short == Some(name))
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
    // An alias documents itself with the help of what it expands to; there
    // is nothing else to say about it.
    let c = spec(alias(name).map_or(name, |(real, _)| real))?;
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
    let aliases = aliases_of(c.name);
    if !aliases.is_empty() {
        let _ = writeln!(out, "\nalso spelled: {}", aliases.join(", "));
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
                "aliases": aliases_of(c.name),
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

/// Fill a tmux `-F` format from one JSON row. `#{title}` reads the field of
/// that name; `#{pane_title}` and `#{window_name}` are tmux's spellings of
/// the same fields, so a tmux format string works as written. A flag prints
/// as 1 or 0 and an unknown field as nothing, as in tmux.
pub fn format_row(format: &str, row: &serde_json::Value) -> String {
    let field = |key: &str| {
        let bare = key
            .strip_prefix("pane_")
            .or_else(|| key.strip_prefix("window_"))
            .unwrap_or(key);
        let bare = bare.strip_suffix("_flag").unwrap_or(bare);
        // A pane row calls its window `window`; a window row calls it `index`.
        let alias = match key {
            "window_index" => "window",
            "window_panes" => "panes",
            "window_name" => "name",
            _ => bare,
        };
        match row
            .get(key)
            .or_else(|| row.get(bare))
            .or_else(|| row.get(alias))
        {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Bool(b)) => u8::from(*b).to_string(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        }
    };
    let mut out = String::new();
    let mut rest = format;
    while let Some(start) = rest.find("#{") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find('}') {
            Some(end) => {
                out.push_str(&field(&rest[start + 2..start + 2 + end]));
                rest = &rest[start + 2 + end + 1..];
            }
            None => {
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// ----------------------------------------------------------------- parsing

/// A window as a script names it: `-t 2` or `-t logs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Win {
    /// Counted from 1, as the status bar shows.
    Index(usize),
    Name(String),
}

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
        /// `None` is the screen only, `Some(None)` the whole scrollback too,
        /// and `Some(Some(n))` the last `n` lines of scrollback.
        history: Option<Option<usize>>,
    },
    Split {
        target: Option<PaneId>,
        dir: Dir,
        /// Run this through the shell instead of starting an interactive one.
        command: Option<String>,
    },
    SelectPane(PaneId),
    /// Zoom a pane, which is a resize to the whole window.
    ZoomPane {
        target: Option<PaneId>,
    },
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
        window: Option<Win>,
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
        format: Option<String>,
    },
    NewWindow {
        name: Option<String>,
        command: Option<String>,
    },
    SelectWindow(Win),
    RenameWindow {
        target: Option<Win>,
        name: String,
    },
    SwapWindow {
        src: Option<Win>,
        dst: Win,
    },
    MoveWindow {
        src: Option<Win>,
        dst: Win,
    },
    KillWindow(Option<Win>),
    ListWindows {
        json: bool,
        format: Option<String>,
    },
    ListSessions {
        json: bool,
    },
    Doctor,
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
/// Split a typed line into words the way a shell would, so the `:` line and
/// a shell agree on `rename-window 'my tab'`. Quotes group; nothing else is
/// special, because a command line is not a shell.
pub fn split(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote = None;
    for c in line.chars() {
        match c {
            '\'' | '"' if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(c),
            ' ' if quote.is_none() => {
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

pub fn parse(verb: &str, args: &[String]) -> Result<Cmd> {
    // An alias is the same command with some of its flags already given.
    if let Some((real, extra)) = alias(verb) {
        let mut all: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        all.extend(args.iter().cloned());
        return parse(real, &all);
    }
    let mut a = Args::new(args);
    // Only the commands that offer `--json` may eat it. Anywhere else it is
    // a stray word, and `end` has to still be able to see it and complain.
    let json = takes_json(verb) && a.flag(&["--json"]);
    if json
        && a.words
            .iter()
            .any(|w| w == "-F" || w.starts_with("--format"))
    {
        bail!("{verb} takes --json or -F, not both");
    }
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
            // tmux's -p prints to stdout, which is the only thing this one
            // does; it is accepted so a tmux script runs as written.
            a.flag(&["-p", "--print"]);
            // tmux's -S is a start line: `-` for the very beginning, `-50` for
            // 50 lines above the screen. Bare -S means all of it.
            let start = a.optional_value(&["-S", "--start"])?;
            let history = match start.as_ref().map(|v| v.as_deref()) {
                None => None,
                Some(None) | Some(Some("-")) => Some(None),
                Some(Some(v)) => match v.trim_start_matches('-').parse() {
                    Ok(n) => Some(Some(n)),
                    Err(_) => bail!("not a line count: {v}"),
                },
            };
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
            Cmd::Split {
                target,
                dir,
                command: a.command(verb)?,
            }
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
                (true, _) => Cmd::ZoomPane { target },
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
            let format = a.value(&["-F", "--format"])?;
            a.end(verb)?;
            Cmd::ListPanes { all, json, format }
        }
        "new-window" => {
            let name = a.value(&["-n", "--name"])?;
            Cmd::NewWindow {
                name,
                command: a.command(verb)?,
            }
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
            let format = a.value(&["-F", "--format"])?;
            a.end(verb)?;
            Cmd::ListWindows { json, format }
        }
        "list-sessions" => {
            a.end(verb)?;
            Cmd::ListSessions { json }
        }
        "doctor" => {
            a.end(verb)?;
            Cmd::Doctor
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
            if spec.trim().is_empty() {
                bail!("run needs an action; `ttmux list-keys` prints them all");
            }
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

    /// A flag whose value may be left out: `Some(None)` for the bare flag,
    /// `Some(Some(v))` for `-S -50` or `--start=-50`. The word after the flag
    /// is its value only when it looks like one: `-`, or digits with an
    /// optional leading `-`.
    fn optional_value(&mut self, names: &[&str]) -> Result<Option<Option<String>>> {
        let looks_like_value = |w: &str| {
            w == "-"
                || w.trim_start_matches('-')
                    .chars()
                    .all(|c| c.is_ascii_digit())
        };
        if let Some(i) = self
            .words
            .iter()
            .position(|w| names.iter().any(|n| w.starts_with(&format!("{n}="))))
        {
            let word = self.words.remove(i);
            let v = word.split_once('=').map(|(_, v)| v).unwrap_or_default();
            return Ok(Some(Some(v.to_string())));
        }
        let Some(i) = self.words.iter().position(|w| names.contains(&w.as_str())) else {
            return Ok(None);
        };
        self.words.remove(i);
        match self.words.get(i) {
            Some(w) if !w.is_empty() && looks_like_value(w) => Ok(Some(Some(self.words.remove(i)))),
            _ => Ok(Some(None)),
        }
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

    /// `-t 2` or `-t logs`: a window by number, counted from 1 as the status
    /// bar counts them, or by name.
    fn window_target(&mut self) -> Result<Option<Win>> {
        self.window_value(&["-t", "--target"])
    }

    fn window_value(&mut self, names: &[&str]) -> Result<Option<Win>> {
        let Some(v) = self.value(names)? else {
            return Ok(None);
        };
        Ok(Some(match v.parse() {
            Ok(n) if n >= 1 => Win::Index(n),
            Ok(_) => bail!("not a window number: {v}"),
            Err(_) => Win::Name(v),
        }))
    }

    /// The rest of the line as one shell command, or nothing. A leftover
    /// flag is a typo, not a program called `-x`; `--` lets one through.
    fn command(mut self, verb: &str) -> Result<Option<String>> {
        match self.words.first().map(String::as_str) {
            Some("--") => {
                self.words.remove(0);
            }
            Some(w) if w.starts_with('-') => bail!("{verb}: unexpected argument {w}"),
            _ => {}
        }
        let c = self.joined();
        Ok((!c.is_empty()).then_some(c))
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
                dir: Dir::Down,
                command: None,
            }
        );
        assert_eq!(
            parsed("split-window -h"),
            Cmd::Split {
                target: None,
                dir: Dir::Right,
                command: None,
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
        assert_eq!(parsed("resize-pane -Z"), Cmd::ZoomPane { target: None });
        assert!(fails("resize-pane 4"));
    }

    #[test]
    fn panes_move_between_windows() {
        assert_eq!(
            parsed("join-pane -s %4 -t 2 -h"),
            Cmd::JoinPane {
                src: Some(4),
                window: Some(Win::Index(2)),
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
                src: Some(Win::Index(1)),
                dst: Win::Index(3)
            }
        );
        assert_eq!(
            parsed("move-window -t 1"),
            Cmd::MoveWindow {
                src: None,
                dst: Win::Index(1)
            }
        );
        assert!(fails("move-window -s 2"));
    }

    #[test]
    fn the_tmux_aliases_mean_their_long_names() {
        assert_eq!(parsed("next-window"), Cmd::Run(Action::NextTab));
        assert_eq!(parsed("last-window"), Cmd::Run(Action::LastTab));
        assert_eq!(
            parsed("lsw"),
            Cmd::ListWindows {
                json: false,
                format: None
            }
        );
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
                target: Some(Win::Index(2)),
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
    }

    #[test]
    fn every_command_parses_its_own_examples() {
        for c in COMMANDS {
            for ex in c.examples {
                let words = split(ex);
                assert_eq!(words[0], "ttmux", "{ex}");
                parse(&words[1], &words[2..]).unwrap_or_else(|e| panic!("{ex}: {e:#}"));
            }
        }
    }

    /// Split on spaces, keeping '...' together, the way a shell would.
    #[test]
    fn json_is_only_swallowed_by_the_commands_that_offer_it() {
        // `-h` is --horizontal to split-window, and --json is a stray word
        // to send-keys. A flag one command owns is text to another.
        assert!(takes_json("list-panes"));
        assert!(
            takes_json("lsp"),
            "an alias inherits the flags it expands to"
        );
        assert!(!takes_json("send-keys"));
        assert!(has_flag("split-window", "-h"));
        assert!(has_flag("join-pane", "-h"));
        assert!(!has_flag("kill-pane", "-h"));

        // --json reaches the pane as text rather than vanishing.
        let Cmd::SendKeys { keys, .. } = parse("send-keys", &split("-l --json")).unwrap() else {
            panic!("not send-keys")
        };
        assert_eq!(keys.len(), "--json".len());
        assert!(parse("kill-pane", &split("--json")).is_err());
    }

    #[test]
    fn a_format_string_reads_the_json_fields_by_either_spelling() {
        let pane = serde_json::json!({
            "id": "%2", "window": 1, "title": "zsh", "active": true
        });
        assert_eq!(
            format_row("#{pane_id} #{title} w#{window_index} #{pane_active}", &pane),
            "%2 zsh w1 1"
        );
        let win = serde_json::json!({
            "index": 3, "name": "logs", "panes": 2, "zoomed": false
        });
        assert_eq!(
            format_row(
                "#{window_index}:#{window_name} #{window_panes} #{window_zoomed_flag}",
                &win
            ),
            "3:logs 2 0"
        );
        assert_eq!(format_row("#{nope}|#{unclosed", &win), "|#{unclosed");
        assert!(fails("list-panes --json -F x"));
    }

    #[test]
    fn a_window_is_named_by_number_or_by_name() {
        assert_eq!(
            parsed("select-window -t 2"),
            Cmd::SelectWindow(Win::Index(2))
        );
        assert_eq!(
            parsed("select-window -t logs"),
            Cmd::SelectWindow(Win::Name("logs".into()))
        );
        // Windows count from 1, so 0 is a mistake rather than a name.
        assert!(fails("kill-window -t 0"));
    }

    #[test]
    fn a_new_pane_or_window_can_run_a_command() {
        let split = |line: &str| match parsed(line) {
            Cmd::Split { command, .. } => command,
            _ => panic!("not split-window"),
        };
        assert_eq!(split("split-window -h"), None);
        assert_eq!(
            split("split-window -h tail -f x.log"),
            Some("tail -f x.log".into())
        );
        assert_eq!(split("split-window -- -weird"), Some("-weird".into()));
        assert!(fails("split-window -x"));
        assert_eq!(
            parsed("new-window -n top htop"),
            Cmd::NewWindow {
                name: Some("top".into()),
                command: Some("htop".into())
            }
        );
    }

    #[test]
    fn capture_pane_reads_tmux_start_lines_and_ignores_print() {
        let hist = |line: &str| match parsed(line) {
            Cmd::CapturePane { history, .. } => history,
            _ => panic!("not capture-pane"),
        };
        assert_eq!(hist("capture-pane"), None);
        assert_eq!(hist("capture-pane -p"), None);
        assert_eq!(hist("capture-pane -S"), Some(None));
        assert_eq!(hist("capture-pane -p -S -"), Some(None));
        assert_eq!(hist("capture-pane -S -50 -t %2"), Some(Some(50)));
        assert_eq!(hist("capture-pane --start=-50"), Some(Some(50)));
        assert!(fails("capture-pane -S soon"));
        assert!(fails("capture-pane -S - extra"));
    }

    #[test]
    fn every_alias_names_a_real_command_and_is_listed_by_it() {
        for (a, real, flags) in ALIASES {
            assert!(spec(real).is_some(), "{a} points at unknown {real}");
            assert!(is_command(a));
            let doc: serde_json::Value = serde_json::from_str(&commands_json()).unwrap();
            let listed = doc["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == *real)
                .and_then(|c| c["aliases"].as_array())
                .is_some_and(|l| l.iter().any(|x| x == a));
            assert_eq!(listed, flags.is_empty(), "{a}");
        }
        assert_eq!(parsed("send hi"), parsed("send-keys hi"));
        assert_eq!(
            parsed("set appearance.gap 1"),
            parsed("set-option appearance.gap 1")
        );
        assert!(help("send-keys").unwrap().contains("also spelled: send"));
    }

    #[test]
    fn zoom_zooms_the_pane_it_was_given() {
        assert_eq!(
            parse("resize-pane", &split("-t %3 -Z")).unwrap(),
            Cmd::ZoomPane { target: Some(3) }
        );
        assert_eq!(
            parse("resize-pane", &split("-Z")).unwrap(),
            Cmd::ZoomPane { target: None }
        );
    }

    #[test]
    fn an_alias_borrows_the_help_of_what_it_expands_to() {
        assert_eq!(help("lsp"), help("list-panes"));
        assert!(help("display").unwrap().contains("display-message"));
    }

    #[test]
    fn run_with_no_action_is_an_error_rather_than_a_no_op() {
        assert!(parse("run", &[]).is_err());
        assert!(parse("run", &split("   ")).is_err());
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
