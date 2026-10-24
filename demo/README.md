# The README demo

This directory makes `docs/demo.avif` (the video on the README), `docs/demo.mp4` (the same video for sites that do not take AVIF), `docs/demo-poster.png` (a still) and the window on `docs/social-preview.png`.

```sh
demo/make.sh            # all steps
demo/make.sh render     # or one step: fonts, clip, record, render, web or social
```

The output goes into `demo/out/`, which git ignores.

## What you need

- macOS. The paths and fonts are macOS paths.
- `ffmpeg` built with `libsvtav1` (Homebrew's build has it).
- `mpv`, `cmatrix` and `nvim` (with nvim-tree on `\F`).
- `claude` (Claude Code), signed in. The demo starts it in the second window.
- The Iosevka Term font (Medium), installed.
- `docs/crabrave.webm`: Crab Rave, which you get yourself. Git does not track it.

## How it works

The demo is not a screen recording. It has two steps, so the video is the same on every run and does not depend on a real terminal or window:

1. **Record.** `record.py` starts the ttmux client in a pty at 170×48 and runs a timeline of keys and mouse drags. It writes an [asciicast v2](https://docs.asciinema.org/manual/asciicast/v2/) file, `out/demo.cast`, with three event types:
   - `o`: the output of the client.
   - `m`: the mouse pointer, as `"x,y,down|up"` in cells (0-based, fractions are allowed).
   - `k`: a label for the keypress badge, for example `⌃⌥→`, `⌃T` or `⇧A`.

   It also writes `out/start.txt`, the time where the video starts. The steps before it (open nvim and the file tree) are not shown.
2. **Render.** `render/` is a small Rust program. It reads the cast into the vendored `vt100` crate and draws each frame with `ab_glyph`, then pipes the frames to ffmpeg (H.264, 30 fps, 2040×1440). The colors are Ghostty's Argonaut palette. The font is Iosevka Term Medium, with fallbacks for symbols. It draws the pointer, and a badge like KeyCastr's in the bottom-right corner.

Then `web` encodes the AVIF and the poster, and `social` puts one frame into the window on the social card.

The session is private: `TTMUX_SESSION=demo` with `ttmux.toml` from this directory. It does not touch your real session. `zshrc` gives a clean prompt with no user name or home path, and no history. `bin/ci` is a fake CI run: it prints Cargo-style output with the real crate and test names, and sends ttmux progress (OSC 7501).

## The timeline

The steps are in `timeline()` in `record.py`:

1. Split right and start `ci` (the pane shows a spinner).
2. Split down and play Crab Rave with `mpv --vo=tct`, as small character video.
3. Split right and start `cmatrix`.
4. Drag panes by their titles onto the sides of other panes to move the splits.
5. Drag a divider to resize.
6. Zoom the CI pane and unzoom it.
7. Free mode: drag the panes as floating windows, then go back to tiles.
8. Press the prefix to see the key hints, then `?` for help, and scroll it.
9. Open the settings and scroll through them.
10. Name the window "demo".
11. Open a second window, "claude", start Claude Code and give it a prompt.
12. Go back to "demo". The "claude" tab shows activity while Claude works.

## Things we learned

- **Answer the terminal probes at once.** The client sends OSC 10, OSC 11 and DA1 queries at startup and waits for the answers. `record.py` answers them with Ghostty's values.
- **Respect synchronized output (DECSET 2026).** The renderer does not take a frame in the middle of an update, so frames do not tear.
- **Draw block, box and braille characters as shapes, not with a font.** If you use a font, `mpv --vo=tct` and `cmatrix` show lines between the cells.
- **The mouse positions are fixed for 170×48.** If you change the size, the layout, or the timeline before a drag, run `record.py --until N --keep` and look at `ttmux capture-pane` to get the new cells. `FREE_MPV` and `FREE_CMX` are for the free-mode drags.
- **Claude runs `sleep` in the background,** so "run sleep 30" ends immediately. We ask it to count tests per file, which keeps it busy for some seconds.
- **A GIF is too large.** The full video as a GIF is 13 to 23 MB, even at 640 px and 10 fps. The AVIF is about 12 MB at 1024 px and 20 fps. Browsers without AVIF show the PNG poster.
- **Put a keyframe in every second of the AVIF.** With one keyframe every 10 s, the browser drops frames and plays slowly after some seconds.
- **ffmpeg times are not QuickTime times.** The frame for the social card is at 36.575 s (`SOCIAL_AT` in `make.sh`), when cmatrix fills its window and the crabs float. At 37.0 s free mode is already off.
- **Live screen capture did not work in the sandbox:** ffmpeg's avfoundation input stopped and did not continue. This is a reason for the record-and-render method.

## How Claude made it

Claude Code made this in one session, with two subagents that worked at the same time:

- **Renderer:** "Write a Rust program that reads an asciicast v2 file (with `o`, `m` and `k` events) into the vendored vt100 crate and renders it to a .mov through ffmpeg. Match Ghostty: Argonaut colors, Iosevka Term Medium, procedural block, box and braille characters, DECSET 2026. Draw the pointer and a keypress badge."
- **Driver:** "Write `record.py`. It runs ttmux in a pty on a private session, plays a timeline of keys and SGR mouse reports, and writes the cast. Show splits, mpv, cmatrix, drags, free mode, help, settings and a second window with Claude Code. Do dry runs and check the screen with `capture-pane` to get the coordinates."

The main session then watched the dry runs, changed the timing from review notes (start later, a smaller video pane, more time on help, show the keys), and chose the encoding.
