# ttmux's vt100

vt100 0.16.2 (https://github.com/doy/vt100-rust, MIT), vendored so ttmux
can fix what its panes run into. Changes from upstream:

- Emoji clusters take the cells a modern terminal gives them: VS16
  (`⚠️`), ZWJ sequences, skin tones and flag pairs share one wide cell.
- Lines scrolled off a region anchored at the top row go to scrollback,
  as in xterm, so apps that pin a footer with DECSTBM keep their history.
- `CSI 3 J` clears the scrollback.
- SGR: underline styles (`4:0`–`4:5`, `21`), underline colour (`58`/`59`),
  strikethrough, blink, hidden, and colon-form colours (`38:2::r:g:b`).
  An unknown `58;…` no longer leaks its arguments as other attributes.
- `CSI f` (HVP) moves the cursor like `CSI H`.
- `Callbacks::unhandled_dcs` hands over DCS strings, for XTGETTCAP.
- `Callbacks::unhandled_osc` is told whether the OSC ended with BEL, so a
  reply can end the same way.
