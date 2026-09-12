use std::process::ExitCode;

use anyhow::{bail, Result};
use ttmux::script;

const USAGE: &str = "\
ttmux — a modern terminal multiplexer

usage: ttmux [options] [command]

commands:
    (none)                attach to the default session, creating it
    new [-d] [-s NAME]    create a session (-d: do not attach)
    attach [-t NAME]      attach to a session (aliases: a, at)
    ls                    list sessions
    kill-session -t NAME  end a session and its panes
    kill-server           end every session

options:
    -c, --config <path>   use this config file instead of the default
        --no-daemon       run in this process, no server (for debugging)
        --print-config    write the default config to stdout and exit
        --where           print the config path and exit
    -h, --help            show this message
    -V, --version         show the version

Everything else is configured from inside ttmux: press ctrl+t ,.

Scripting commands act on the session named by $TTMUX_SESSION, which every
pane already has set:
";

/// The session `ttmux` with no `-t` means. `$TTMUX_SESSION` is what the app
/// itself already uses for the name it shows, so the two must agree.
fn default_session() -> String {
    std::env::var("TTMUX_SESSION").unwrap_or_else(|_| "main".into())
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            // `{e:#}` so the cause chain shows: "bind <path>" alone is not a
            // diagnosis.
            eprintln!("ttmux: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let mut args = std::env::args().skip(1);
    let mut no_daemon = false;
    let mut verb = None;
    let mut rest: Vec<String> = vec![];

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}\n{}", script::usage());
                return Ok(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("ttmux {}", env!("CARGO_PKG_VERSION"));
                return Ok(ExitCode::SUCCESS);
            }
            "--where" => {
                println!("{}", ttmux::config::config_path().display());
                return Ok(ExitCode::SUCCESS);
            }
            "--print-config" => {
                let cfg = ttmux::config::Config::default();
                print!("{}", toml::to_string_pretty(&cfg)?);
                return Ok(ExitCode::SUCCESS);
            }
            "--no-daemon" => no_daemon = true,
            // The app reads the path from the environment, so this just sets
            // it before startup.
            "-c" | "--config" => match args.next() {
                Some(p) => std::env::set_var("TTMUX_CONFIG", p),
                None => bail!("{arg} needs a path"),
            },
            other if other.starts_with('-') => bail!("unknown option {other}\n\n{USAGE}"),
            other => {
                verb = Some(other.to_string());
                rest.extend(args);
                break;
            }
        }
    }

    if no_daemon {
        ttmux::app::run()?;
        return Ok(ExitCode::SUCCESS);
    }

    // Bare `ttmux` creates the session it attaches to; a spelled-out
    // `attach` does not, so a typo in `-t` is an error and not a new session.
    let create = verb.is_none();
    match verb.as_deref().unwrap_or("attach") {
        "new" | "new-session" => {
            let name = target(&rest, &["-s", "-t"])?.unwrap_or_else(default_session);
            if rest.iter().any(|a| a == "-d") {
                ttmux::server::spawn(&name)?;
            } else {
                ttmux::client::attach(&name, true)?;
            }
        }
        "attach" | "attach-session" | "a" | "at" => {
            let name = target(&rest, &["-t", "-s"])?.unwrap_or_else(default_session);
            ttmux::client::attach(&name, create)?;
        }
        "ls" | "list-sessions" => {
            ttmux::proto::cleanup_stale();
            for (name, path) in ttmux::proto::list_sessions() {
                let live = if ttmux::proto::is_live(&path) {
                    "live"
                } else {
                    "stale"
                };
                println!("{name}: {live}");
            }
        }
        "kill-session" => {
            let name = target(&rest, &["-t", "-s"])?
                .ok_or_else(|| anyhow::anyhow!("kill-session needs -t NAME"))?;
            ttmux::client::kill(&name)?;
        }
        "kill-server" => {
            for (name, path) in ttmux::proto::list_sessions() {
                if ttmux::proto::is_live(&path) {
                    ttmux::client::kill(&name)?;
                }
            }
            ttmux::proto::cleanup_stale();
        }
        // Scripting: hand the words to the session and print what it says.
        // The verbs and their arguments are tmux's, so `-t` here names a
        // pane or a window, not a session.
        other if script::is_command(other) => {
            return Ok(script_command(other, rest));
        }
        other => bail!("unknown command {other}\n\n{USAGE}\n{}", script::usage()),
    }
    Ok(ExitCode::SUCCESS)
}

/// Run one scripting command and print the answer. Two of them never need a
/// session: `--help` is documentation, and `list-commands` is how an agent
/// learns the API before anything is running.
fn script_command(verb: &str, rest: Vec<String>) -> ExitCode {
    // An alias has no help of its own, so it falls through to the session,
    // which knows what it expands to.
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        if let Some(text) = script::help(verb) {
            print!("{text}");
            return ExitCode::SUCCESS;
        }
    }
    if verb == "list-commands" {
        if rest.iter().any(|a| a == "--json") {
            print!("{}", script::commands_json());
        } else {
            print!("{}", script::usage());
        }
        return ExitCode::SUCCESS;
    }

    let argv: Vec<String> = std::iter::once(verb.to_string()).chain(rest).collect();
    let (code, text) = match ttmux::client::command(&default_session(), &argv) {
        Ok(r) => r,
        Err(e) => (script::EXIT_ERROR, format!("{e:#}")),
    };
    if !text.is_empty() {
        if code == script::EXIT_OK {
            print!("{}", ends_with_newline(text));
        } else {
            eprintln!("ttmux: {}", text.trim_end());
        }
    }
    ExitCode::from(code)
}

/// Listings already end in a newline; a one-line answer does not.
fn ends_with_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The session name a verb was given, e.g. `-t work`. `-d` is a flag, not a
/// name, so it is skipped rather than rejected.
fn target(rest: &[String], names: &[&str]) -> Result<Option<String>> {
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        if names.contains(&arg.as_str()) {
            return match it.next() {
                Some(name) => Ok(Some(name.clone())),
                None => bail!("{arg} needs a session name"),
            };
        }
        if arg != "-d" {
            bail!("unexpected argument {arg}\n\n{USAGE}");
        }
    }
    Ok(None)
}
