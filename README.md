<h1 align="center">ttmux</h1>

<p align="center"><em>A modern multiplexer alternative.</em></p>

<p align="center">
  <a href="https://github.com/statico/ttmux/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/statico/ttmux/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/statico/ttmux/releases"><img alt="Release" src="https://img.shields.io/github/v/release/statico/ttmux?include_prereleases&sort=semver"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Rust 1.82+" src="https://img.shields.io/badge/rust-1.82%2B-orange.svg">
  <img alt="Platforms" src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg">
</p>

<p align="center"><img alt="ttmux demo" src="docs/demo.gif" width="700"></p>

> [!WARNING]
> **Beta software.** ttmux is still under test. Expect bugs, and expect the
> config format to change before 1.0.

> [!NOTE]
> **Made entirely with [Claude Code](https://claude.com/claude-code).** Every
> line of this project was written by Claude.

ttmux runs many terminals in one window, like tmux or screen. It adds
floating panes, real mouse support, curved borders, and a settings screen
that you open inside the app.

## Install

With Homebrew, on macOS or Linux:

```
brew install statico/tap/ttmux
```

Or build it from source, which needs Rust 1.82 or later:

```
cargo install --git https://github.com/statico/ttmux
```

Then start it:

```
ttmux
```

Prebuilt binaries for macOS and Linux are also on the
[releases page](https://github.com/statico/ttmux/releases).

## Why

tmux works well and has thirty years of muscle memory behind it. ttmux keeps
the parts that work and replaces the parts that do not.

| | tmux | ttmux |
|---|---|---|
| Config | `.tmux.conf`, reload by hand | TOML, edited in the app with <kbd>ctrl+t</kbd> <kbd>,</kbd> |
| Layout | tiling only | tiling **or** free-floating, drag and drop |
| Borders | single-line ASCII | curved, square, heavy, double, dashed, or one tmux-style divider |
| Mouse | bolted on | click, drag, resize, and wheel |
| Agents | — | a per-pane busy, needs-you, or done mark |

## Keys

The prefix is <kbd>ctrl+t</kbd>. The pane keys follow the window commands of
vim. You can rebind all of them.

| Key | Action |
|---|---|
| <kbd>ctrl+t</kbd> <kbd>v</kbd> / <kbd>s</kbd> | Split right or down |
| <kbd>ctrl+t</kbd> <kbd>h</kbd><kbd>j</kbd><kbd>k</kbd><kbd>l</kbd> | Move the focus |
| <kbd>ctrl+t</kbd> <kbd>H</kbd><kbd>J</kbd><kbd>K</kbd><kbd>L</kbd> | Resize the pane |
| <kbd>ctrl+alt+f</kbd> | Switch between tiling and free mode |
| <kbd>ctrl+t</kbd> <kbd>f</kbd> | Float one pane |
| <kbd>ctrl+t</kbd> <kbd>z</kbd> | Zoom the focused pane |
| <kbd>ctrl+t</kbd> <kbd>c</kbd> / <kbd>n</kbd> / <kbd>p</kbd> | New, next, or previous tab |
| <kbd>ctrl+t</kbd> <kbd>&lt;</kbd> / <kbd>&gt;</kbd> | Move this tab left or right |
| <kbd>ctrl+t</kbd> <kbd>!</kbd> | Break the pane out into its own tab |
| <kbd>ctrl+t</kbd> <kbd>@</kbd> | Join the pane into another tab |
| <kbd>ctrl+t</kbd> <kbd>:</kbd> | The command line |
| <kbd>ctrl+alt+n</kbd> | Go to the next pane that wants you |
| <kbd>ctrl+t</kbd> <kbd>shift+a</kbd> / <kbd>a</kbd> | Rename the tab, or the pane |
| <kbd>ctrl+t</kbd> <kbd>,</kbd> | Settings |
| <kbd>ctrl+t</kbd> <kbd>?</kbd> | Help |
| <kbd>ctrl+t</kbd> <kbd>q</kbd> / <kbd>Q</kbd> | Close the pane, or quit ttmux |

Press <kbd>ctrl+t</kbd> <kbd>?</kbd> in the app for the full list.

Hold the prefix and a popup lists what can follow it, like which-key in
neovim. Turn it off with `general.which-key = false`.

<kbd>ctrl+t</kbd> <kbd>:</kbd> opens a command line for the scripting
commands below. <kbd>Tab</kbd> completes a command or a flag, and the row
above shows the usage of the command you are typing.

A name you type is yours. A program can set a title with an escape
sequence, but that title never replaces a name you set, and it names the
pane, not the tab. The tab takes the title only while it holds one pane.

Every text field takes the readline keys: <kbd>ctrl+a</kbd>,
<kbd>ctrl+e</kbd>, <kbd>ctrl+w</kbd>, <kbd>ctrl+k</kbd>, <kbd>ctrl+u</kbd>,
<kbd>alt+b</kbd>, and <kbd>alt+f</kbd>.

## Free mode

<kbd>ctrl+alt+f</kbd> turns the grid of panes into a window manager. Drag a
title bar to move a pane. Drag any corner to resize it. Click to raise it.
Tiling mode gets the same mouse support for dividers. Grab the line between
two panes and drag it.

## Sessions

ttmux works like tmux. The server keeps your panes alive after you detach.

```
ttmux                       # attach to the default session, or start it
ttmux new -d -s work        # start a session in the background
ttmux attach -t work        # attach to a named session
ttmux ls                    # list the sessions
ttmux kill-session -t work  # stop one
```

<kbd>ctrl+t</kbd> <kbd>d</kbd> detaches. A new binary starts a new server, so
an upgrade never kills a running session. Detach, install, then attach again
with the old binary until you are ready to move.

## Scripting

Every pane can drive the session it lives in. The commands are tmux's, so a
tmux script mostly runs unchanged.

```
ttmux send-keys -t %2 "make test" Enter   # type into a pane
ttmux split-window -h                     # split side by side
ttmux new-window                          # open a tab
ttmux select-pane -t %2                   # or -L -R -U -D
ttmux list-panes                          # %2: [80x24] zsh (active)
ttmux list-windows                        # 1:build (2 panes) (active)
ttmux rename-window build                 # name a window
ttmux display-message "done"              # show text in the status bar
ttmux join-pane -s %3 -t 2                # move a pane into window 2
ttmux break-pane                          # and back out into its own
ttmux run toggle-zoom                     # any key-binding action
```

A target is a pane id from `list-panes`, or a window number counted from 1.
With no target the command acts on the focused pane. The session is the one
`$TTMUX_SESSION` names, which every pane already has set. `ttmux --help`
prints the full list.

[API.md](API.md) is the full reference: every command, every flag, the exit
codes, and recipes. `ttmux <command> --help` prints one command, and
`ttmux list-commands --json` prints the whole API for an agent to read.

## Config

The file is `~/.config/ttmux/ttmux.toml`. ttmux writes it for you on the
first save. Every key in it is also in the settings screen, so the file is
for version control and the screen is for changes.

ttmux watches the file. When you save it, ttmux reloads it. A restart is not
necessary.

```toml
[general]
mouse = true
scrollback = 10000
free-mode = false
keys-preset = "vim"          # vim | tmux | screen
passthrough-images = true    # kitty, iTerm2, and sixel inline images

[appearance]
border-style = "curved"      # curved | square | heavy | double | dashed | divider | none
border-focused = "#7aa2f7"
gap = 0

[status.footer]
enabled = true
left = ["session", "mode"]
center = ["tabs"]
right = ["agents", "time"]

[agents]
enabled = true
bell-on-attention = true

# Overrides on top of the preset. "none" unbinds a key.
[keys]
"ctrl+t v" = "split right"
"ctrl+t &" = "none"
```

## Custom widgets

A widget runs a shell command and shows what it prints. Give it a name under
`[status.widgets]`, then put that name in a row.

```toml
[status.footer]
enabled = true
right = ["disk", "time"]

[status.widgets.disk]
command = "df -h / | awk 'NR==2 {print $5}'"
interval = 60               # seconds between runs, 0 runs it once
```

The command runs with `sh -c`, so pipes, globs and `~` work. ttmux uses the
first line of the output. tmux markup such as `#[fg=red,bold]` is honored, so
a script written for tmux `status-right` works without a change.

A slow command does not block the screen. It runs in its own thread, and ttmux
does not start a second copy while the first one runs.

## Coding-agent alerts

A pane that runs a coding agent gets a mark: `◐` for busy, `●` for wants
you, and `✓` for done. The status bar counts the panes that wait on you, and
<kbd>ctrl+alt+n</kbd> goes to the next one. The patterns live under
`[agents]`, so this works with any agent.

## Terminals

ttmux is built against Ghostty and iTerm2. Any terminal with 24-bit color
and SGR mouse reporting works, including Alacritty, kitty, WezTerm, and
Terminal.app.

Ghostty never sends `alt+left` and `alt+right` to ttmux, because it binds
them to the word motions of readline. To get horizontal focus movement back,
unbind them in the config of Ghostty:

```
keybind = alt+left=unbind
keybind = alt+right=unbind
```

## Platforms

macOS on Apple silicon and Intel, and Linux on x86_64 and aarch64. CI builds
and tests all three.

## Development

```
make check      # format, lint, and test, the same as CI
make help       # all the targets
```

MIT licensed.
