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
| Config | `.tmux.conf` DSL, reload by hand | TOML, edited in-app with <kbd>ctrl+t</kbd> <kbd>,</kbd> |
| Layout | tiling only | tiling **or** free-floating, drag and drop |
| Borders | single-line ASCII | curved, square, heavy, double, dashed |
| Mouse | bolted on | first class: click, drag, resize, wheel |
| Agents | — | per-pane busy / needs-you / done indicators |

## Keys

Prefix is <kbd>ctrl+t</kbd>, and the pane keys follow vim's window commands. Everything below is rebindable.

| Key | Does |
|---|---|
| <kbd>ctrl+t</kbd> <kbd>v</kbd> / <kbd>s</kbd> | split right / down (<kbd>%</kbd> and <kbd>"</kbd> also work) |
| <kbd>ctrl+t</kbd> <kbd>h</kbd><kbd>j</kbd><kbd>k</kbd><kbd>l</kbd> | move focus (<kbd>alt</kbd>+arrows too) |
| <kbd>ctrl+t</kbd> <kbd>H</kbd><kbd>J</kbd><kbd>K</kbd><kbd>L</kbd> | resize (<kbd>alt+shift</kbd>+arrows too) |
| <kbd>ctrl+alt+f</kbd> | toggle tiling ↔ free mode |
| <kbd>ctrl+t</kbd> <kbd>f</kbd> | float just this pane |
| <kbd>ctrl+t</kbd> <kbd>space</kbd> | cycle tiling presets |
| <kbd>ctrl+t</kbd> <kbd>z</kbd> | zoom the focused pane |
| <kbd>ctrl+t</kbd> <kbd>c</kbd> / <kbd>n</kbd> / <kbd>p</kbd> | new / next / previous tab |
| <kbd>ctrl+alt+n</kbd> | jump to the next pane wanting attention |
| <kbd>ctrl+t</kbd> <kbd>ctrl+t</kbd> / <kbd>t</kbd> | last tab / send the prefix to the pane |
| <kbd>ctrl+t</kbd> <kbd>=</kbd> | even out the tiling |
| <kbd>ctrl+t</kbd> <kbd>,</kbd> | settings |
| <kbd>ctrl+t</kbd> <kbd>?</kbd> | help |
| <kbd>ctrl+t</kbd> <kbd>q</kbd> / <kbd>Q</kbd> | close the pane / quit ttmux |

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
mouse = true                 # set false to turn mouse support off entirely
scrollback = 10000
free-mode = false
keys-preset = "vim"          # vim | tmux | screen
passthrough-images = true    # kitty / iTerm2 / sixel inline images

[appearance]
border-style = "curved"      # curved | square | heavy | double | dashed | none
border-focused = "#7aa2f7"
gap = 0

# Header and footer are independent; enable either, both or neither.
[status]
effect = "flat"              # flat | starfield | gradient

[status.header]
enabled = false
left = ["host"]
center = ["tabs"]
right = ["session"]

[status.footer]
enabled = true
left = ["session", "mode"]
center = ["tabs"]
right = ["agents", "time"]

[agents]
enabled = true
bell-on-attention = true

# `keys` are overrides on top of the preset. "none" unbinds.
[keys]
"ctrl+t v" = "split right"
"alt+left" = "focus left"
"ctrl+t &" = "none"
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

On Ghostty the default `alt+left` and `alt+right` never arrive: Ghostty binds
them to the readline word motions and rewrites them before any encoding
happens, so ttmux cannot see them. Unbind them in Ghostty's config to get
horizontal focus movement back:

```
keybind = alt+left=unbind
keybind = alt+right=unbind
```

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
