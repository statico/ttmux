# Changelog

## Unreleased

- Copy mode, as in tmux: select text with the keyboard or a mouse drag,
  and paste it back with the prefix and ]. It is off by default; set
  `general.copy-mode = true`. When it is off, the copy mode key scrolls
  back as before.
- `ttmux upgrade` moves a live session to a new server. The shells stay.
  Use it after you install a new version. On macOS, also use it when the
  terminal that started the session has quit: the keychain and permission
  prompts work again. `ttmux doctor` shows if a session has this problem.
  A session from 0.5 or older cannot move. End it with the old binary.
- `general.update-environment`: new panes get `SSH_AUTH_SOCK` and similar
  variables from the client that attached last. If that client does not
  have one of them, new panes do not get it either.

## 0.5.0 — 2026-09-12

- The config no longer reloads when the file changes. Press the prefix and
  r after editing it. Widget commands in it run as you, so a program that
  writes the file should not get them run unasked; tmux also waits for
  `source-file`.
- Only images are replayed to your terminal: kitty images that would make
  it read a file or shared memory, and iTerm2 sequences other than inline
  files, are dropped.
- `general.clipboard` and `general.notifications` turn off OSC 52 copies
  and desktop notifications from panes. Both stay on by default.
- The tabs in the middle of the status bar stay put when the agent marks
  on the right come and go.
- SECURITY.md compares ttmux with a default tmux.

## 0.4.1 — 2026-09-12

- `ttmux` refuses to attach from inside a pane, as tmux does: into its own
  session it would draw itself inside itself, and for any other session
  `unset TTMUX` forces it. `$TTMUX` in a pane is now the session's socket.
- The tmux keymap sends its prefix with ctrl+b ctrl+b, and the screen
  keymap with ctrl+a a (rename-pane moves to ctrl+a .), for sessions nested
  over ssh.

## 0.4.0 — 2026-09-12

- Programs that ask the terminal where the cursor is, like fzf's ctrl+r,
  get an answer, so they draw at once instead of after the next key.
- Panes get answers to the other queries programs block on: device
  attributes, version, window size, mode reports (DECRQM) and the
  terminal's foreground and background, so neovim, fish and delta pick
  the right theme and stop waiting.
- Synchronized output (mode 2026) holds a pane's redraw until it is
  complete, with a one second limit.
- Focus events reach the focused pane on pane switches and when the
  terminal window gains or loses focus.
- Each pane keeps its own cursor shape, and detaching restores yours.
- OSC 52 copies reach your clipboard, and notifications (OSC 9, 99, 777)
  reach your terminal.
- A paste can no longer end its own bracket early.
- Emoji with skin tones, flags, ZWJ sequences and VS16 take one wide
  cell instead of breaking the line up.
- Scrollback keeps the lines a program scrolls away under a fixed footer,
  as Codex and Claude Code draw, and `CSI 3J` clears it.
- Undercurls and dotted, dashed and double underlines keep their style
  and colour, and blink, hidden and strikethrough are kept too.
- Programs that ask for the kitty keyboard protocol or modifyOtherKeys get
  it, so shift+enter, ctrl+enter and ctrl+i reach them as distinct keys.
- Panes can ask whether the theme is dark or light (`CSI ?996n`, mode
  2031), and XTGETTCAP answers for truecolor, undercurl, cursor shape,
  OSC 52 and synchronized output.
- Colour replies end the way the question did, BEL or ST.
- Video through kitty graphics (`mpv --vo=kitty`) plays instead of printing
  base64: chunked images are replayed whole, a stray escape cancels an
  image the way terminals do, and a big image no longer disconnects you.
- A performance test guards parsing, drawing and redraw size; `make
  bench` prints the numbers.

## 0.3.1 — 2026-09-12

- The settings screen has an About section with the version and the
  project URL.
- Settings and help look cleaner: colour swatches beside hex values, dots
  for switches, spaced leaders, and hints that light up their keys.
- Every text field takes ctrl+y (yank), ctrl+t (transpose), alt+backspace
  and alt+u/l/c on top of the readline keys it already had.
- ctrl+n and ctrl+p move through every list, and ctrl+g cancels an overlay.
- The which-key popup paints its labels on its own background and keeps
  its shadow off the status bar.

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
