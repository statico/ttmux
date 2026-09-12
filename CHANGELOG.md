# Changelog

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
- macOS on Apple silicon and Intel, and Linux on x86_64 and aarch64.
