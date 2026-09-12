# Changelog

## 0.3.1 — 2026-09-12

- The settings screen has an About section with the version and the
  project URL.

## 0.3.0 — 2026-09-12

- Scripting API: `ttmux send-keys`, `split-window`, `select-pane`,
  `list-panes`, `rename-window`, `display-message`, `run ACTION` and more,
  with tmux's names and targets. `ttmux <command> --help` documents one
  command, `ttmux list-commands --json` documents all of them, and the exit
  code says what happened: 0 ran, 1 failed, 2 bad arguments, 3 no session.
  A command with no `-t` acts on the pane the script runs in, as tmux reads
  `$TMUX_PANE`. `split-window`, `new-window` and `break-pane` print the id
  of what they made, and `split-window` and `new-window` take a command to
  run. `-t` names a window by number or by name. `list-panes` and
  `list-windows` take tmux's `-F '#{pane_id}'` format strings. `capture-pane -p` and
  `-S -` work as they do in tmux.
  [API.md](API.md) is the full reference.
- Panes move between tabs: `break-pane` (<kbd>ctrl+t</kbd> <kbd>!</kbd>),
  `join-pane` (<kbd>ctrl+t</kbd> <kbd>@</kbd>), `swap-pane`, and
  `move-tab left`/`right` (<kbd>ctrl+t</kbd> <kbd><</kbd> and <kbd>></kbd>).
- A which-key popup. Hold the prefix and ttmux lists what can follow it.
  Turn it off with `general.which-key = false`.
- A command line on <kbd>ctrl+t</kbd> <kbd>:</kbd>, with Tab completion and
  a usage hint for every scripting command.
- The help overlay scrolls, so a long keymap no longer runs off the bottom.
- SECURITY.md describes the threat model and the hardening.
- The config file is written readable only by you. Widget commands live in
  it and ttmux runs them with `sh -c`.

## 0.2.0 — 2026-09-11

- Custom status widgets: a widget runs a shell command on an interval and
  shows what it prints.
- Config hot reloading. ttmux watches the file and reloads it when you save.
- A divider border style: one shared line between panes, like tmux.
- Pick a colour by dragging across the grid in the settings screen.
- `rename-pane` names a pane. A name you set is never replaced by a title
  the program sets, and a program's title names the pane, not the tab,
  unless the tab holds one pane.
- Every text field takes the readline keys: ctrl+a, ctrl+e, ctrl+w, ctrl+k,
  ctrl+u, alt+b, alt+f, and the rest.
- Keystroke latency in a busy pane is down from 9.6ms to 1.9ms.
- Panes are told the terminal takes 24-bit colour, so a program no longer
  falls back to the 16 ANSI colours.

## 0.1.0 — 2026-09-11

The first release. Beta software.

- Tiling panes and free-floating panes, with drag and drop in both modes.
- Mouse support: click to focus, drag a divider or a corner to resize, and
  wheel to scroll.
- A settings screen and a command palette inside the app. The config file is
  TOML at `~/.config/ttmux/ttmux.toml`.
- Border styles: curved, square, heavy, double, dashed, and none.
- A client and server split, so a session survives a detach and an upgrade.
- Status bar widgets in a header, a footer, or both.
- Coding-agent marks per pane, and a key that goes to the next pane that
  wants you.
- Inline images pass through for the kitty, iTerm2, and sixel protocols.
- Three keybinding presets: vim, tmux, and screen.
- A Homebrew formula: `brew install statico/tap/ttmux`.
- macOS on Apple silicon and Intel, and Linux on x86_64 and aarch64.
