# Scripting API

This is the full reference for the ttmux scripting commands. Use it to drive
a running session from a shell script, a Makefile, or a coding agent.

The mental model is one sentence: a command is the same thing a key binding
is. Both make the same change to the running app. Anything a key does, a
command can do, and `ttmux run` reaches the actions that have no command of
their own.

The names and short flags follow tmux, so a tmux script mostly runs
unchanged.

Three ways to reach these commands:

- `ttmux <command>` from a shell, inside or outside a pane.
- <kbd>ctrl+t</kbd> <kbd>:</kbd> inside ttmux. Tab completes a command or a
  flag, and the row above shows its usage.
- `ttmux list-commands --json` for an agent that wants the whole API first.

`ttmux <command> --help` prints one command. `--help` and `list-commands`
need no running session.

## How a command is addressed

Every command goes to one session. The session is the one `$TTMUX_SESSION`
names. Every pane already has it set, so a script inside a pane needs no
target. Outside a pane the default is `main`.

Inside the session, `-t` picks what the command acts on:

| Command takes | What `-t` accepts | With no `-t` |
|---|---|---|
| A pane | a pane id from `list-panes`, such as `%3` or plain `3` | the focused pane |
| A window | a window number, counted from 1 | the current window |

Every short flag has a long spelling. `-t` is `--target`, and `-s` is
`--source`. The value can follow the flag or join it with `=`, so
`--target %2` and `--target=%2` are the same. Flag order does not matter.

## Output and exit codes

A command prints a human table by default. Where `--json` is offered, it
prints machine-readable JSON instead. Errors go to stderr with a `ttmux:`
prefix.

| Exit code | Meaning |
|---|---|
| 0 | the command ran |
| 1 | the command failed, for example no such pane |
| 2 | the arguments are wrong, and nothing ran |
| 3 | no session is listening |

## Panes

### send-keys

```
ttmux send-keys [-t PANE] [-l] KEY...
```

Type into a pane, as if the keys were pressed there.

A word is a key name such as `Enter` or `C-c` when it matches one. Otherwise
ttmux types the word character by character. Both `C-c` and `ctrl+c` work.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |
| `-l`, `--literal` | send the words as text, even when one is a key name |

```
ttmux send-keys 'make test' Enter
ttmux send-keys --target %2 C-c
ttmux send-keys -l Enter
```

### capture-pane

```
ttmux capture-pane [-t PANE] [-S]
```

Print what a pane shows, for reading a build or a test run.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |
| `-S`, `--history` | include the scrollback, not only the visible screen |

```
ttmux capture-pane
ttmux capture-pane -t %2 --history
```

### split-window

```
ttmux split-window [-t PANE] [-h] [-v]
```

Split a pane in two.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |
| `-h`, `--horizontal` | side by side |
| `-v`, `--vertical` | one above the other, the default as in tmux |

```
ttmux split-window -h
ttmux split-window -t %1 -v
```

### select-pane

```
ttmux select-pane [-t PANE] [-L] [--next]
```

Focus a pane, by id or by direction. Give a target or a direction, but not
both.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |
| `-L`, `--left` | focus the pane to the left, and `-R`, `-U`, `-D` for the rest |
| `--next` | focus the next pane in order, or `--prev` for the one before |

```
ttmux select-pane -t %3
ttmux select-pane -R
```

### resize-pane

```
ttmux resize-pane [-t PANE] [-L] [-Z] [N]
```

Grow a pane, or zoom it to fill the window. `N` is the number of cells, and
the default is 2.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |
| `-L`, `--left` | the edge to push, and `-R`, `-U`, `-D` for the rest |
| `-Z`, `--zoom` | toggle zoom instead of resizing |

```
ttmux resize-pane -R 10
ttmux resize-pane -Z
```

### swap-pane

```
ttmux swap-pane [-s PANE] [-t PANE]
```

Exchange the places of two panes.

| Flag | Meaning |
|---|---|
| `-s`, `--source PANE` | the pane to move. Default is the focused one |
| `-t`, `--target PANE` | the pane to swap it with |

```
ttmux swap-pane -s %1 -t %2
```

### join-pane

```
ttmux join-pane [-s PANE] [-t N] [-h]
```

Move a pane into another window. `move-pane` is another spelling of this
command.

| Flag | Meaning |
|---|---|
| `-s`, `--source PANE` | the pane to move. Default is the focused one |
| `-t`, `--target N` | the window to move it into. Default is the current one |
| `-h`, `--horizontal` | place it side by side rather than below |

```
ttmux join-pane -s %4 -t 1 -h
```

### break-pane

```
ttmux break-pane [-t PANE]
```

Move a pane into a new window of its own.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |

```
ttmux break-pane
ttmux break-pane -t %2
```

### rename-pane

```
ttmux rename-pane [-t PANE] NAME
```

Name a pane. The name outlives any title the program sets.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |

```
ttmux rename-pane build
ttmux rename-pane -t %2 logs
```

### kill-pane

```
ttmux kill-pane [-t PANE]
```

Close a pane.

| Flag | Meaning |
|---|---|
| `-t`, `--target PANE` | the pane to act on. Default is the focused one |

```
ttmux kill-pane -t %3
```

### list-panes

```
ttmux list-panes [-a] [--json]
```

One line per pane.

| Flag | Meaning |
|---|---|
| `-a`, `--all` | every window, not only the current one |
| `--json` | print JSON instead of a table |

```
ttmux list-panes
ttmux list-panes -a --json
```

## Windows

### new-window

```
ttmux new-window [-n NAME]
```

Open a window.

| Flag | Meaning |
|---|---|
| `-n`, `--name NAME` | name it as it is created |

```
ttmux new-window -n logs
```

### select-window

```
ttmux select-window [-t N] [-n]
```

Focus a window.

| Flag | Meaning |
|---|---|
| `-t`, `--target N` | the window to act on, counted from 1. Default is the current one |
| `-n`, `--next` | the next window, `-p` the one before, `-l` the last one |

```
ttmux select-window -t 2
ttmux select-window -n
```

### rename-window

```
ttmux rename-window [-t N] NAME
```

Name a window.

| Flag | Meaning |
|---|---|
| `-t`, `--target N` | the window to act on, counted from 1. Default is the current one |

```
ttmux rename-window build
```

### swap-window

```
ttmux swap-window [-s N] [-t N]
```

Exchange the places of two windows.

| Flag | Meaning |
|---|---|
| `-s`, `--source N` | the window to move. Default is the current one |
| `-t`, `--target N` | the window to swap it with |

```
ttmux swap-window -s 1 -t 3
```

### move-window

```
ttmux move-window [-s N] [-t N]
```

Renumber a window, sliding the others along.

| Flag | Meaning |
|---|---|
| `-s`, `--source N` | the window to move. Default is the current one |
| `-t`, `--target N` | the position it takes |

```
ttmux move-window -s 3 -t 1
```

### select-layout

```
ttmux select-layout NAME
```

Arrange the panes. `NAME` is `even-horizontal`, `even-vertical`,
`main-vertical`, or `main-horizontal`.

This command takes no flags.

```
ttmux select-layout main-vertical
```

### kill-window

```
ttmux kill-window [-t N]
```

Close a window and every pane in it.

| Flag | Meaning |
|---|---|
| `-t`, `--target N` | the window to act on, counted from 1. Default is the current one |

```
ttmux kill-window -t 2
```

### list-windows

```
ttmux list-windows [--json]
```

One line per window.

| Flag | Meaning |
|---|---|
| `--json` | print JSON instead of a table |

```
ttmux list-windows --json
```

## Session

### list-sessions

```
ttmux list-sessions [--json]
```

One line per session on this machine.

| Flag | Meaning |
|---|---|
| `--json` | print JSON instead of a table |

```
ttmux list-sessions
```

### display-message

```
ttmux display-message TEXT
```

Show text in the status bar.

This command takes no flags.

```
ttmux display-message 'tests passed'
```

### show-options

```
ttmux show-options [--json] [KEY]
```

Print the config, or one dotted key of it.

| Flag | Meaning |
|---|---|
| `--json` | print JSON instead of a table |

```
ttmux show-options
ttmux show-options appearance.gap
```

### set-option

```
ttmux set-option KEY VALUE
```

Write one dotted key into the config file, which reloads at once.

This command takes no flags.

```
ttmux set-option appearance.border_style divider
ttmux set-option appearance.gap 1
```

### list-keys

```
ttmux list-keys [--json]
```

Every key binding and the action it runs.

| Flag | Meaning |
|---|---|
| `--json` | print JSON instead of a table |

```
ttmux list-keys --json
```

### list-commands

```
ttmux list-commands [--json]
```

Every scripting command, its flags and its examples.

| Flag | Meaning |
|---|---|
| `--json` | print JSON instead of a table |

```
ttmux list-commands --json
```

### run

```
ttmux run ACTION...
```

Run any key-binding action by name, such as `toggle-zoom`. This reaches the
actions that have no command of their own. `ttmux list-keys` prints the
names.

This command takes no flags.

```
ttmux run toggle-zoom
ttmux run 'select-tab 2'
```

## Recipes

### Run a build in a second pane and read the result

```
ttmux split-window -h
ttmux list-panes
ttmux send-keys -t %2 'make check' Enter
sleep 60
ttmux capture-pane -t %2 --history > /tmp/build.log
ttmux display-message 'build log is in /tmp/build.log'
```

### Set up a three-pane workspace in one script

```
#!/bin/sh
ttmux new-window -n work
ttmux rename-pane editor
ttmux split-window -h
ttmux rename-pane -t %2 server
ttmux split-window -v
ttmux rename-pane -t %3 logs
ttmux select-layout main-vertical
ttmux send-keys -t %2 'npm run dev' Enter
ttmux send-keys -t %3 'tail -f log/dev.log' Enter
ttmux select-pane -t %1
```

### Drive a long-running agent pane and watch for output

```
#!/bin/sh
ttmux split-window -v
ttmux rename-pane -t %2 agent
ttmux send-keys -t %2 'claude' Enter
ttmux send-keys -t %2 'fix the failing test' Enter
while ! ttmux capture-pane -t %2 | grep -q 'Done'; do
  sleep 5
done
ttmux display-message 'the agent finished'
ttmux select-pane -t %2
```

### Move a pane into another window

```
ttmux list-windows
ttmux list-panes -a
ttmux join-pane -s %4 -t 1 -h
ttmux select-window -t 1
ttmux select-layout even-horizontal
```

### Park a pane in its own window, then bring it back

```
ttmux break-pane -t %3
ttmux rename-window scratch
ttmux move-window -t 1
ttmux join-pane -s %3 -t 2
ttmux select-window -t 2
```

### Zoom one pane for a demo, then restore the layout

```
ttmux select-pane -t %2
ttmux resize-pane -Z
ttmux set-option appearance.gap 1
sleep 300
ttmux run toggle-zoom
ttmux set-option appearance.gap 0
```

## For agents

- `ttmux list-commands --json` prints the whole surface: every command, its
  flags, its examples, and the exit codes. Read it once instead of guessing.
- `ttmux <command> --help` prints one command: what it does, its flags, and
  its examples.
- Use `--json` for anything you parse. A table is for people, and its shape
  can change.
- Check the exit code after every command. A failure prints to stderr and
  exits non-zero, so a script that ignores the code keeps going on a broken
  pane.
- Read a pane with `capture-pane` rather than guessing what a program did.
- `$TTMUX_SESSION` names the session you are in. Keep it set when you run
  ttmux from a child process.
