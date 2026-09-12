# Contributing

## Pull Requests Are Disabled — Please File An Issue

This project does not accept pull requests. It is faster to tell a coding agent what
to do than it is to review someone else's code, so contributions come in as
descriptions of the problem rather than as diffs.

File an issue instead, for both bug reports and feature requests. Issues can be as
long and as descriptive as you want — there is no such thing as too much detail here.
A thorough issue is worth more than a patch.

### A good ttmux issue

- What you did, what happened, and what you expected instead.
- Your ttmux version (`ttmux -V`), your terminal, and your platform.
- The exact steps that reproduce it, and a screenshot if it is a drawing problem.
- Your config, or the part of it that matters. `ttmux --where` prints the path.
- The daemon log. It lives next to the socket; `TTMUX_LOG=/tmp/ttmux.log ttmux`
  puts it somewhere you choose.

## Guiding Principles

- In all your interactions with developers, maintainers, and users, be kind.
- Prefer small, comprehensible changes over large sweeping ones. Individual commits
  should be meaningful atomic chunks of work.
- Every change must be fully understood and explicitly endorsed by a human before it
  lands. AI assistance is great, and this repo is optimized for it, but we keep
  quality by keeping our agents on track to write clear code, useful (not useless)
  tests, good architecture, and big-picture thinking.
- No change should introduce new failing lint, format, test, or build results.
- Every change starts from an issue or discussion thread.
- Terminal behavior gets a test. The rendering and pty paths are where regressions
  hide, and they are cheap to cover.

## Local Development

```bash
make build      # debug build
make run        # build and run ttmux
make test       # the whole test suite
make help       # every target
```

Run ttmux without its server when you need a debugger or a backtrace on stderr:

```bash
cargo run -- --no-daemon
```

## Quality Checks

Run the full suite before handing off a change. This is exactly what CI runs:

```bash
make check      # cargo fmt --check, clippy with -D warnings, and cargo test
```

## Releases

Bump `version` in `Cargo.toml`, add a section to `CHANGELOG.md`, then:

```bash
make tag        # runs the checks, tags v<version>, and pushes it
```

The tag starts `.github/workflows/release.yml`. The workflow builds the four
supported targets, attaches the tarballs to the release, and then updates the
Homebrew formula in [statico/homebrew-tap](https://github.com/statico/homebrew-tap).

The formula installs those same tarballs, so it needs no build step. To update
the tap by hand, or after a release that failed part way:

```bash
scripts/update-tap.sh v0.1.0
```

The workflow needs a `TAP_GITHUB_TOKEN` secret, because the token that GitHub
gives a workflow can only write to its own repository. Use a fine-grained
personal access token with contents write permission on the tap repository:

```bash
gh secret set TAP_GITHUB_TOKEN --repo statico/ttmux
```
