<p align="center">
  <img src="assets/changehog-header-image.png" alt="changehog: watch your changes, live" width="640">
</p>

A live, auto-following diff viewer for watching coding agents (Claude Code,
etc.) work. Run it in a split pane next to the agent: it shows what changed,
scrolls to the latest edit, and flips between files as they change.

<img width="1272" height="667" alt="diff-live-2026-10-07-short" src="https://github.com/user-attachments/assets/193831fc-de86-4e8e-857f-b6e08b5e1fa3" />

```bash
cargo run --release -- [PATH] [--base session|head]
```

- `--base session` (default): diff against the working tree as it was when
  changehog started. Changes stay visible even if the agent commits.
- `--base head`: diff against HEAD at startup, including pre-existing changes.
- `--cycle SECONDS`: how long to show each file when cycling (default 4).
- `--wrap` / `--no-wrap`: wrap long lines instead of cutting them off at the
  edge (default off).

It cycles through the changed files continuously, jumping ahead to files as
they change. When a file's changes don't fit on screen, scrolling down to the
next off-screen change is a step in the cycle, at the same interval. Until the session has changes of its own, it shows uncommitted
changes against HEAD, or if the tree is clean, the last commit.

## Configuration

Settings are read from `~/.config/changehog/config.toml` (or
`$XDG_CONFIG_HOME/changehog/config.toml`) if it exists, and command-line flags
override them. To start one with every setting at its default:

```bash
mkdir -p ~/.config/changehog && changehog --print-config > ~/.config/changehog/config.toml
```

| Setting | Default | |
|---|---|---|
| `cycle_seconds` | `4.0` | Seconds per file or scroll step (0.5 to 600) |
| `base` | `"session"` | `"session"` or `"head"` |
| `resume_after_seconds` | `20.0` | Idle time before auto-follow resumes (1 to 3600) |
| `wrap` | `false` | Wrap long lines |
| `sidebar` | `"auto"` | `"auto"`, `"open"` or `"closed"` |

Use `--config PATH` to read a different file. Changes made while running
(speed, sidebar, wrap) aren't saved back to the file.

## Controls

Keys: `j/k` scroll, `d/u` page, `n/p` next/previous file, `f` toggle follow,
`+/-` cycle faster/slower, `s` toggle the file sidebar, `w` toggle soft wrap,
`q` quit.

Mouse: click a file in the sidebar to jump to it, click `«` / `»` to collapse
or expand the sidebar, and use the wheel to scroll. Since changehog captures
the mouse, hold Option (macOS) or Shift (most Linux terminals) to select text.

Any navigation pauses auto-follow; it resumes after 20s idle.

## Layout

- `crates/core`: frontend-agnostic engine
  - `git`: read-only git access (`GIT_OPTIONAL_LOCKS=0`, so it never takes
    `index.lock` while an agent is running git)
  - `baseline`: what each file is compared against
  - `watch`: filesystem events, coalesced into bursts
  - `diff`: baseline→current diffs, with lines from the latest edit flagged `fresh`
  - `session`: ties it together; emits `SessionEvent`s on a channel
  - `director`: which file to show and when to flip (dwell shrinks as the backlog grows)
  - `timeline`: per-file revision history, for future replay and catch-up
- `crates/tui`: ratatui frontend

## Dev note

`target/` is marked ignored for Dropbox (`com.dropbox.ignored` and
`com.apple.fileprovider.ignore#P` xattrs). If it's deleted, re-create it and
re-apply the attributes, or builds will sync.

## License

MIT. See [LICENSE](LICENSE).
