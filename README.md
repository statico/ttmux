# ttmux

A modern terminal multiplexer in Rust. Tiling *and* free-floating panes, real
mouse support, curved borders, coding-agent alerts, and a config you edit from
inside the app instead of from a man page.

```
cargo install --path .
ttmux
```

## Why

tmux is excellent and thirty years of muscle memory deep. ttmux keeps the parts
that work (prefix keys, splits, tabs) and drops the parts that don't:

| | tmux | ttmux |
|---|---|---|
| Config | `.tmux.conf` DSL, reload by hand | TOML, edited in-app with <kbd>ctrl+a</kbd> <kbd>s</kbd> |
| Layout | tiling only | tiling **or** free-floating, drag and drop |
| Borders | single-line ASCII | curved, square, heavy, double, dashed |
| Mouse | bolted on | first class: click, drag, resize, wheel |
| Agents | — | per-pane busy / needs-you / done indicators |

## Keys

Prefix is <kbd>ctrl+a</kbd>. Everything below is rebindable.

| Key | Does |
|---|---|
| <kbd>ctrl+a</kbd> <kbd>%</kbd> / <kbd>"</kbd> | split right / down |
| <kbd>alt</kbd>+arrows | move focus |
| <kbd>alt+shift</kbd>+arrows | resize the focused pane |
| <kbd>ctrl+alt+f</kbd> | toggle tiling ↔ free mode |
| <kbd>ctrl+a</kbd> <kbd>f</kbd> | float just this pane |
| <kbd>ctrl+a</kbd> <kbd>space</kbd> | cycle tiling presets |
| <kbd>ctrl+a</kbd> <kbd>z</kbd> | zoom the focused pane |
| <kbd>ctrl+a</kbd> <kbd>c</kbd> / <kbd>n</kbd> / <kbd>p</kbd> | new / next / previous tab |
| <kbd>ctrl+alt+n</kbd> | jump to the next pane wanting attention |
| <kbd>ctrl+a</kbd> <kbd>s</kbd> | settings |
| <kbd>ctrl+a</kbd> <kbd>?</kbd> | help |

## Free mode

<kbd>ctrl+alt+f</kbd> turns the pane grid into a window manager. Drag a title
bar to move a pane, drag its bottom-right corner to resize, click to raise.
Tiling mode gets the same mouse handling for dividers: grab the line between
two panes and drag.

## Config

`~/.config/ttmux/ttmux.toml`, written for you on first save. Everything in it
is reachable from the settings UI, so the file is for version control and the
UI is for fiddling.

```toml
[general]
mouse = true
scrollback = 10000
free-mode = false

[appearance]
border-style = "curved"      # curved | square | heavy | double | dashed | none
border-focused = "#7aa2f7"
gap = 0

[status]
position = "bottom"
left = ["session", "mode"]
center = ["tabs"]
right = ["agents", "time"]

[agents]
enabled = true
bell-on-attention = true

[keys]
"ctrl+a %" = "split right"
"alt+left" = "focus left"
```

## Coding-agent alerts

Panes running a coding agent get a status glyph: `◐` busy, `●` wants you, `✓`
done. The status bar counts the panes waiting on you and <kbd>ctrl+alt+n</kbd>
jumps to the next one. Detection is pattern-based and configurable under
`[agents]`, so it works with whatever agent you run.

## Terminals

Developed against Ghostty and iTerm2; anything with 24-bit colour and SGR mouse
reporting works, including Alacritty, kitty, WezTerm and Terminal.app (which
falls back to 256 colours).

## Platforms

macOS on Apple silicon and Intel, and Linux (x86_64 and aarch64). CI builds and
tests all three.

## Development

```
cargo test           # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

MIT licensed.
