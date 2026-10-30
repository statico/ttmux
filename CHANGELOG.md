# Changelog

## 1.1.2 — 2026-10-30

- The cursor in a text field is now the character under it, shown in
  reverse colors. Before, the cursor was a thin bar that took a column
  of its own and moved the text after it to the right. With the cursor
  at the start, for example after <kbd>ctrl+a</kbd> in the rename
  prompt, the name looked like it had a space in front of it. Nothing
  was added to the name. This change applies to every text field: the
  rename prompt, the command palette, the pane switcher, the command
  line, the settings screen, and the color picker.

## 1.1.1 — 2026-10-29

- The `starfield` status bar effect twinkles now. Before, it put new
  stars in new places every second. Now each star stays in its cell and
  fades in and out slowly, on its own timing, so the bar looks calm.
  The idea comes from the starfield in the Codex CLI.

## 1.1.0 — 2026-10-29

New ways to move between tabs and panes.

- The prefix and a digit from 1 to 9 go to that tab, as `prefix N` does
  in tmux, and 0 goes to the tenth tab. This works in every keymap.
- <kbd>ctrl+1</kbd> to <kbd>ctrl+9</kbd> and <kbd>ctrl+0</kbd> go to a
  tab with no prefix. The new `general.tab-number-keys` setting, also on
  the settings screen, changes the modifier to `alt` or `ctrl-shift`, or
  turns the keys `off`. The ctrl keys need a terminal that has the kitty
  keyboard protocol. Other terminals send ctrl+1 as a plain `1`, and
  that still types `1`.
- <kbd>ctrl+{</kbd> and <kbd>ctrl+}</kbd> (ctrl+shift+[ and ]) go to the
  previous and next tab, as in a browser. They also need the kitty
  keyboard protocol.
- The help screen and the which-key popup show the ten tab keys as one
  row.
- <kbd>ctrl+'</kbd> opens a list of every tab and pane, with each pane's
  title and folder. Type to filter it, and press <kbd>Enter</kbd> to go to
  the tab or pane. The letters you type only need to come in order. The
  new `choose-pane` action is also on the prefix and <kbd>"</kbd> (Modern
  and screen keymaps) and the prefix and <kbd>w</kbd> or <kbd>s</kbd>
  (tmux keymap). In the Modern keymap, the prefix and <kbd>"</kbd> opened
  the command palette before; the prefix and <kbd>;</kbd> still does.
- The command palette uses the same matching, so `nt` finds `new-tab`.
  Its list now scrolls to keep the selected row on screen.
- As in tmux, a command can be shortened to the start of its name, if
  no other command starts the same way. `ttmux att` attaches, and
  `ttmux kill-ses -t work` ends a session. A start that fits more than
  one command, such as `kill-s`, is an error that lists them.
- Saving in the settings screen says so in the screen itself: "saved"
  and the path, or why the save failed. Before, the only sign was a
  message in the status bar, behind the settings screen.

## 1.0.1 — 2026-10-23

This release has the same code as 1.0.0.

- ttmux is on crates.io. Install it with `cargo install ttmux`. The
  vt100 fork that ttmux uses is published as the `ttmux-vt100` crate.
- The README is rewritten in plain English, and its install steps give
  `cargo install ttmux`.

## 1.0.0 — 2026-10-23

ttmux is out of beta. This release has the same code as 0.10.1.

- The README and `SECURITY.md` no longer call ttmux beta software. Only
  the newest release gets fixes, as before.
- The demo video is also in the repo as an MP4 (`docs/demo.mp4`), with a
  PNG still (`docs/demo-poster.png`), for sites that do not show AVIF.

## 0.10.1 — 2026-10-23

- `cargo install --git https://github.com/statico/ttmux` works again. In
  0.10.0 it failed with "multiple packages with binaries found", because
  the renderer for the demo video was a second binary in the repo. It is
  now a cargo example, which `cargo install` does not count.

## 0.10.0 — 2026-10-22

- Drag a tab along the status bar to reorder the tabs. When you press a
  tab, it is selected and drawn as held (accent-coloured, bold and
  underlined) until you let go. A tab moves only when the pointer would
  still be on it after the move, so tabs of different widths do not swap
  back and forth. While you hold a tab, the panes get no mouse events.
- The README has a new demo video, and the social preview image uses a
  frame from it. `demo/` holds the scripts that make them.

## 0.9.0 — 2026-10-19

Thanks to @benjmin-r and @justinstimatze, whose issues asked for every
change in this release.

- A pane one row high no longer panics when its text wraps, as a client
  started on a 0x0 pty did (#2).
- Moving focus with a direction goes back to the pane it came from when
  that pane is a candidate, as in tmux. `last-pane` (`ctrl+b ;` in the
  tmux preset, or `select-pane -l`) jumps to it (#3).
- Copy mode moves by vi words with `w`, `b` and `e` (#4).
- Scripts can tag panes with tmux's `@` options:
  `set-option -p -t %3 @slot 3`. They show in `list-panes` and in `-F`
  formats as `#{@slot}`, and they survive an upgrade (#5).
- `list-panes` gives each pane's `pid` and `cwd` (`#{pane_pid}`,
  `#{pane_current_path}`) (#6).
- `general.remain-on-exit` keeps a pane on screen after its program
  exits, marked `dead` in `list-panes`, and `respawn-pane [-k] [COMMAND]`
  starts it again in the same tile (#7).
- `split-window -d` and `new-window -d` leave focus where it is (#8).
- A key can run a command when it is pressed, through the `command`
  action, and `select-pane --where @slot=3` finds a pane by its option:
  `"alt+3" = "command select-pane --where @slot=3"` (#9).
- Dependencies are updated.

## 0.8.1 — 2026-10-13

- A client on a slow link no longer freezes on its last frame while its
  keys still reach the panes. Since the tab spinner (0.7.0) a few seconds
  of stall while an agent was busy filled its queue, and the server
  stopped drawing to it for good. Now it skips the frames it missed and
  gets one repaint when it catches up; one the server gives up on is
  disconnected, so it says so.
- `ttmux doctor` waits for the keychain check again; 0.8.0 gave it no time.

## 0.8.0 — 2026-10-12

- Programs can report their own status with OSC 7501. `working`,
  `blocked`, `done` and `error` set the pane's agent mark in place of the
  `[agents]` patterns, and the next shell prompt (OSC 133 `A`) or a reset
  ends them. The pane's border shows the report's progress and message,
  and what a blocked program waits for.
- An agent rings the bell at most once every three seconds, so one that
  flips in and out of needing you cannot flood the terminal.
- The README lists each terminal spec a pane can count on.
- A full reset (`ESC c`) turns off focus events, theme reports, the kitty
  keyboard flags and modifyOtherKeys, and puts back the default cursor.

## 0.7.0 — 2026-10-08

- A tab with a busy agent in it shows a spinner, so it keeps turning in a
  background tab. Claude Code stops animating its own title when it loses
  focus. Pick the style with `agents.busy-spinner`: `braille` (the
  default), `circle`, `line`, `dots` or `none`.

## 0.6.4 — 2026-09-20

- No change to what ttmux does. The code now says why a question from a
  pane must be answered in ttmux, and not passed out to the terminal.

## 0.6.3 — 2026-09-20

- A pane no longer shows `Gi=31;OK` at all. ttmux answers a program's
  kitty graphics question itself. The answer used to come from the outer
  terminal, and the client typed it into the pane as keys.

## 0.6.2 — 2026-09-19

- A pane no longer fills with `Gi=31;OK`. A program's kitty graphics
  question was asked again after every frame, and each answer from the
  terminal arrived as input in the pane that asked.

## 0.6.1 — 2026-09-19

- A pane no longer fills with `Gi=31;OK` for good. When a pane echoed the
  terminal's answer to a kitty graphics query, ttmux sent it back to the
  terminal, which answered again.

## 0.6.0 — 2026-09-18

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
