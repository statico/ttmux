# Security

## Threat model

ttmux runs one user's terminal session on that user's own machine. It
protects the control socket, the session state, and the child processes from
other users on the same machine. The uid that starts the server is the trust
boundary. ttmux is not a privilege boundary between users, and it is not a
sandbox. Any process that runs as you can connect to the socket and get a
shell, because `ttmux send-keys` types into a pane. Treat a ttmux session as
equal to a login shell for your account.

## Supported versions

Only the newest release gets fixes. ttmux is beta software before 1.0, so
there is no backport branch.

| Version | Supported |
|---|---|
| 0.5.x | Yes |
| 0.4.x and older | No |

## Report a vulnerability

Open a private security advisory at
https://github.com/statico/ttmux/security/advisories/new. The repository has
no security email address. Do not open a public issue for a vulnerability.

## Hardening practices

### The control socket

- The socket directory is `$XDG_RUNTIME_DIR`, `$TMPDIR`, or `/tmp`, plus
  `ttmux-<uid>`. See `socket_dir` in `src/proto.rs`.
- ttmux creates the directory with mode `0700` and sets the mode to `0700`
  again on every start.
- ttmux reads the directory with `symlink_metadata` and refuses to continue
  when the owner uid is not yours. A wrong owner on a shared `/tmp` is a
  hijack, so tightening the mode is not enough.
- The socket file gets mode `0600` right after `bind`. See `spawn` in
  `src/server.rs`.
- A session name that is empty, or that holds `/`, `..`, or a NUL byte, is
  rejected in `socket_path`.
- The socket name carries the protocol version. A new binary starts its own
  server, so an upgrade never hands an old client a new wire format.
- `is_live` connects to test a socket. `cleanup_stale` deletes the sockets of
  servers that are gone, and `spawn` removes a stale file before it binds.
- The daemon does `fork`, `setsid`, `fork` again. It has no controlling
  terminal and it cannot get one. stdin and stdout go to `/dev/null`. stderr
  goes to a log file that ttmux opens with mode `0600`.

### The wire protocol

- Frames are a 4-byte little-endian length, then JSON. See `src/proto.rs`.
- `MAX_FRAME` is 96 MiB, room for a large inline image. `read_msg` checks the length prefix and returns an
  error before it allocates the body.
- A short read at a frame boundary is a clean EOF. A partial frame is an
  `UnexpectedEof` error, so "peer detached" and "peer died" stay apart.
- Both sides check the protocol number. The server answers a mismatched
  `Hello` with `Error` and `Bye`, then closes. The client refuses any message
  that arrives before `Welcome`.
- A client gets 5 seconds to send `Hello`. A connection that says nothing
  loses its thread.
- Each client has a 256-frame queue and its own writer thread. The app thread
  never blocks on a socket. A client that stops reading fills its queue and is
  dropped.
- Writes time out after 5 seconds during the handshake and after 2 seconds
  once the client is attached.
- A scripted command waits at most 5 seconds for the app thread. The client
  side waits 10 seconds for the reply and 5 seconds for `kill-server`.
- The server parses a scripted command before it reaches the app thread. A
  parse error goes back to the script and never touches the session.

### Child processes and the pty

- A pane runs the shell from `general.shell`, else `$SHELL`, else `/bin/sh`.
  See `Pane::spawn` in `src/pty.rs`.
- `portable_pty` calls `setsid` before `exec`, so each pane child is its own
  process group and session leader.
- `Pane::kill` sends `SIGHUP` to the process group, kills the child, sends
  `SIGKILL` to the group, and then runs `pkill -9 -s <sid>` to catch a
  grandchild in a job-control group of its own. It then calls `wait` to reap.
- `Drop for Pane` calls `kill`, so every exit path cleans up. A `reaped` flag
  stops a second signal reaching a recycled pid.
- `pump` feeds the emulator at most 4 MiB per pass, so a noisy pane cannot
  starve the UI. A pane holds at most 32 pending images.
- A program can set a pane title with an escape sequence. ttmux strips every
  control character from that title before it stores it.
- A pane gets `TERM`, `COLORTERM`, `TTMUX` (the socket path), `TTMUX_PANE`,
  and `TTMUX_SESSION` on top of your environment.

### Config and widgets

- The config file is `$TTMUX_CONFIG`, else `$XDG_CONFIG_HOME/ttmux/ttmux.toml`,
  else `$HOME/.config/ttmux/ttmux.toml`. See `config_path` in `src/config.rs`.
- A missing file gives the defaults. A parse error is reported and the
  defaults load, so a bad edit never locks you out of the session.
- ttmux reads the file at start and again only when you ask: the
  `reload-config` key (prefix <kbd>r</kbd>), a save from the settings
  screen, or `ttmux set-option`. It does not watch the file, so a program
  that rewrites it cannot get a widget command run behind your back. tmux
  also waits for `source-file`.
- ttmux writes the file with mode `0600`. See `write_private` in
  `src/config.rs`.
- A custom widget is a shell command. ttmux runs it with `sh -c`, with stdin
  on `/dev/null` and stderr dropped. See `src/widget.rs`.
- Each widget runs on its own thread. An `AtomicBool` swap stops a second
  copy of a widget that still runs.
- Widget output goes through the tmux markup parser and into the cell grid.
  ratatui drops graphemes that hold control characters, so a widget cannot
  write escape sequences to your terminal.
- A custom widget name never shadows a built-in widget. See `src/status.rs`.

### Terminal escape sequences

- `src/graphics.rs` captures only three framings: kitty `ESC _ G … ESC \`,
  iTerm2 `ESC ] 1337 ; …`, and sixel `ESC P … q … ESC \`.
- Only sequences that draw are replayed. See `is_safe` in `src/graphics.rs`.
  A kitty image must carry its data inline (`t=d`): `t=f`, `t=t`, and `t=s`
  would make your terminal read a file or shared memory, and are dropped.
  An iTerm2 sequence must be `File=` with `inline=1`: downloads, user
  variables, focus stealing, and the rest of OSC 1337 are dropped.
- Past that check ttmux does not interpret the payload. It replays the exact
  bytes at the pane cursor.
- One captured sequence is capped at 4 MiB, and a chunked kitty image at
  64 MiB once joined. Past a cap ttmux drops the image, swallows the rest to
  its terminator, and carries on.
- `passthrough-images = false` in `[general]` turns replay off.
- OSC 52 copies reach your clipboard, and OSC 9, 99 and 777 notifications
  reach your terminal. `clipboard = false` and `notifications = false` in
  `[general]` turn each off. A pane that asks to paste gets its own last
  copy back, never your real clipboard.
- With all three off, no pane bytes reach your terminal outside the cell
  grid.

### Compared with tmux

ttmux aims to be at least as strict as a default tmux.

| | tmux default | ttmux default |
|---|---|---|
| Socket | `0700` directory, owner only | the same |
| A pane scripting its session | allowed (`send-keys`, `run-shell`) | allowed (`send-keys`, `split-window <cmd>`) |
| Config that runs commands | `run-shell`, `#()`, and plugins, read at start or on `source-file` | widgets and `shell`, read at start or on reload |
| Image passthrough | off (`allow-passthrough`) | on, drawing sequences only |
| OSC 52 from a program | ignored (`set-clipboard external`) | copied, `clipboard = false` to ignore |
| Notifications | not forwarded | forwarded, `notifications = false` to drop |

The two defaults that differ are there because neovim over ssh, image
viewers, and coding agents need them, and each has a switch.

### Dependencies and builds

- The direct dependency list in `Cargo.toml` is small: `anyhow`, `crossterm`,
  `ratatui`, `portable-pty`, `vt100`, `serde`, `serde_json`, `toml`,
  `unicode-width`, and `libc`. `vt100` is a fork kept in `vendor/vt100`,
  with its changes listed in `vendor/vt100/TTMUX.md`.
- `Cargo.lock` is in the repository, so a source build is reproducible at the
  same lock file.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test --all-targets`, and a release build. It runs on macOS arm64,
  macOS x86_64, and Linux x86_64. Releases also build Linux aarch64.
- The release workflow builds four targets and writes a `.sha256` file for
  each archive with `shasum -a 256`.
- The `publish` job takes `contents: write` and nothing more. The Homebrew
  tap job uses a separate token, because the default `GITHUB_TOKEN` cannot
  push to another repository.

## Known limits

ttmux does not defend against these.

- **Another process running as you.** The socket permissions are the only
  control. There is no token and no per-client authentication. Any process
  with your uid connects and runs `send-keys`, which is arbitrary code in
  your shell. Root gets the same access. This includes a program in a pane,
  so a sandboxed coding agent in one pane can type into an unsandboxed shell
  in another. Keep the socket directory (`ttmux-<uid>` under
  `$XDG_RUNTIME_DIR`, `$TMPDIR`, or `/tmp`) out of an agent sandbox.
- **`$TTMUX_SOCKET`.** The override wins outright. It skips the session-name
  check and the `0700` directory check. Point it only at a path you own.
- **The config file.** ttmux treats it as trusted input and runs its widget
  commands and `shell` without asking. A program that can write the file,
  such as a coding agent allowed to edit files, gets those commands run at
  the next start or reload. `ttmux set-option` writes the same keys over the
  socket. Keep `~/.config/ttmux` out of what an agent may write, as you
  would `~/.zshrc`.
- **The pane environment.** A pane child inherits your full environment. ttmux
  adds `TERM`, `COLORTERM`, `TTMUX`, `TTMUX_PANE`, and `TTMUX_SESSION`. It
  removes nothing, so secrets in your environment reach every pane.
- **Image decoders in your terminal.** A replayed image is untrusted data
  that your terminal decodes. Set `passthrough-images = false` when you run
  untrusted output.
- **Clipboard writes.** With `clipboard` on, any program in a pane can
  replace your clipboard, for you to paste somewhere later.
- **Terminal escape sequences inside a pane.** vt100 renders pane output, and
  ttmux does not audit that emulator for parser bugs.
- **A hung widget.** After 30 seconds ttmux frees the widget slot but leaves
  the process running. It never kills the command.
- **Memory-unsafe code.** `src` holds about 20 `unsafe` blocks for `libc`
  calls: `fork`, `setsid`, `killpg`, `dup2`, and the signal handler. There is
  no `#![forbid(unsafe_code)]`.
- **Supply chain scanning.** CI runs no `cargo audit` and no `cargo deny`.
  There is no Dependabot config. GitHub Actions are pinned to a major tag,
  not to a commit SHA.
- **Release provenance.** Release binaries are not signed and carry no build
  attestation. The `.sha256` files sit next to the archives in the same
  release, so they detect a corrupt download and not a compromised release.
- **Beta status.** ttmux is before 1.0. The wire format and the config format
  change between releases.
