# Ferric Commander

A dual-pane terminal file manager in Rust. No async runtime: the UI thread
renders on a fixed 16 ms budget and hands every slow operation to an
`std::thread` that reports back over an `mpsc` channel.

The authoritative specification is `Ferric_Commander_Specification.txt`. Section 7
of that document records the design amendments and the decisions taken before
implementation began.

## Build

    cargo build --release
    ./target/release/ferric-commander

Rust 1.85+ is required (edition 2024). Tested on rustc 1.99.1.

## Keys

| Key         | Action                        |
|-------------|-------------------------------|
| `j` / `Down`| Move cursor down              |
| `k` / `Up`  | Move cursor up                |
| `Tab`       | Switch active pane            |
| `Enter`     | Enter directory, or open file |
| `Backspace` | Go to parent directory        |
| `Space`     | Select row, advance cursor    |
| `a`         | Invert selection              |
| `F1` / `t`  | Cycle theme                   |
| `F2` / `s`  | Cycle sort order              |
| `F10` / `q` | Quit                          |

On a regular file, `Enter` suspends the TUI and runs `$EDITOR` (falling back to
`$VISUAL`, then `vi`), then restores the screen and re-reads both panes. On a
directory it navigates; on a dangling symlink it reports on the status line.

Copy, move, rename and delete arrive in Phases 3 and 4.

## Layout

    src/main.rs        terminal lifecycle (raw mode, alternate screen, panic hook), event loop
    src/app.rs         state machine, key routing
    src/theme.rs       colour palettes
    src/fs/panel.rs    directory listing, sorting, cursor and selection
    src/ui/mod.rs      frame layout: two panes, status line, key bar
    src/ui/panel_view.rs  row rendering

## Status

- Phase 1 complete: terminal bootstrap, themes, synchronous filesystem engine,
  navigation, sorting, selection.
- Phase 2 complete: `Table`-based panes with automatic scroll-into-view, empty
  directory hint, `$EDITOR` launch with suspend/restore. 30 tests, clippy clean.
- Phase 3 next: modal system (Confirm, Input, ErrorReport), in-place rename.
- Phase 4: threaded copy/move/delete with progress and cancellation.

## Design notes worth reading

- **Symlinks are never followed during traversal.** Listing uses
  `symlink_metadata`, so a link is reported as a link and a dangling one as
  broken; only interactive navigation resolves a link, exactly as a shell does.
- **Selection is keyed by path, not row index**, so a refresh that reorders or
  removes rows cannot silently retarget an operation.
- **Failures never move the cursor's directory.** A pane lists the target
  before committing `cwd`, so entering an unreadable directory returns an error
  and leaves the pane usable.
