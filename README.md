# Ferric Commander

A dual-pane terminal file manager in Rust. No async runtime: the UI thread
renders on a fixed 16 ms budget and hands every slow operation to an
`std::thread` that reports back over an `mpsc` channel.

The authoritative specification is `Ferric_Commander_Specification.txt`. Section 7
of that document records the design amendments and the decisions taken before
implementation began.

Released under the MIT licence.

## Build

    cargo build --release
    ./target/release/ferric-commander

Rust 1.85+ is required (edition 2024). Tested on rustc 1.99.1.

    cargo test            # 75 tests
    cargo clippy --all-targets -- -D warnings

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
| `F5` / `c`  | Copy selection to other pane  |
| `F6` / `m`  | Move selection to other pane  |
| `F7` / `r`  | Rename the row under cursor   |
| `F8` / `d`  | Delete selection              |
| `F10` / `q` | Quit                          |

On a regular file, `Enter` suspends the TUI and runs `$EDITOR` (falling back to
`$VISUAL`, then `vi`), then restores the screen and re-reads both panes. On a
directory it navigates; on a dangling symlink it reports on the status line.

Rename opens a pre-filled prompt; `Enter` applies, `Esc` discards. Renaming onto
an existing name asks for confirmation instead of silently replacing it.

Copy, move and delete run on a worker thread and show a progress dialog.

### Transfers

`F5`/`F6`/`F8` act on the selection, or on the row under the cursor when nothing
is selected, and always target the other pane's directory. Nothing touches the
disk before a confirmation:

- No clashes: "Copy 3 items" with `Enter` to proceed.
- Clashes: the dialog names the conflicting entries (up to three) and offers
  `Enter` to overwrite or `s` to skip. Overwriting a directory merges into it
  instead of replacing it.
- Delete has its own prompt, with no skip option.

While a task runs a dialog shows the current file, item and byte counters and a
percentage. `Esc` requests cancellation: the dialog changes to "Cancelling…" and
stays up until the worker acknowledges, which happens within one 64 KiB chunk.
A cancelled copy deletes its own partial file and leaves every completed item in
place. Failures on individual items are collected and reported together at the
end instead of stopping the batch.

## Layout

    src/main.rs        terminal lifecycle (raw mode, alternate screen, panic hook), event loop
    src/app.rs         state machine, key routing
    src/theme.rs       colour palettes
    src/fs/panel.rs    directory listing, sorting, cursor and selection
    src/ui/mod.rs      frame layout: two panes, status line, key bar
    src/ui/panel_view.rs  row rendering
    src/ui/modal.rs    confirm / input / error / progress dialogs
    src/tasks/mod.rs   task, update and handle types: the UI/worker contract
    src/tasks/worker.rs  scan, copy, move, delete, cancellation
    src/format.rs      shared size and path formatting

## Status

- Phase 1 complete: terminal bootstrap, themes, synchronous filesystem engine,
  navigation, sorting, selection.
- Phase 2 complete: `Table`-based panes with automatic scroll-into-view, empty
  directory hint, `$EDITOR` launch with suspend/restore.
- Phase 3 complete: modal system (Confirm, Input, ErrorReport, Progress view),
  in-place rename with UTF-8-safe editing and overwrite confirmation.
- Phase 4 complete: worker thread with a pre-flight byte/item scan, 64 KiB
  chunked copies with per-chunk cancellation checks, EXDEV-safe moves, conflict
  prompts with overwrite/skip, throttled progress, and abort cleanup. 75 tests,
  clippy clean.

## Design notes worth reading

- **Symlinks are never followed during traversal.** Listing uses
  `symlink_metadata`, so a link is reported as a link and a dangling one as
  broken; only interactive navigation resolves a link, exactly as a shell does.
- **Selection is keyed by path, not row index**, so a refresh that reorders or
  removes rows cannot silently retarget an operation.
- **Failures never move the cursor's directory.** A pane lists the target
  before committing `cwd`, so entering an unreadable directory returns an error
  and leaves the pane usable.
- **Modals trap every key.** A `q` typed into a rename prompt is a `q`, not a
  quit; the routing branches before anything else looks at the key.
- **The rename cursor counts characters, not bytes**, so editing `café.txt` or
  `🦀.txt` cannot split a code point. `InputState::byte_index` is the single
  place the two representations meet.
- **The worker never blocks the UI.** It owns an unbounded channel and a
  `try_recv`-only control channel; the UI drains at most 100 messages per frame,
  so a task cannot starve rendering even while it reports every 50 ms.
- **A move deletes each source only after that source's copy succeeded**, and
  only for the source that just finished — a cross-device move that dies
  halfway leaves the un-copied sources intact.
- **Cancellation is cooperative and per chunk.** `Esc` sets a flag; the copy
  loop checks it every 64 KiB, deletes the half-written destination and reports
  `Aborted`. Nothing is killed, so no lock or buffer is left inconsistent.
- **The worker treats a disconnected control channel as a cancel**, so quitting
  the UI mid-copy stops the writes instead of orphaning a thread that keeps
  filling the disk.

## Licence

MIT — see [LICENSE](LICENSE). In short: use it, change it, ship it, sell it;
keep the copyright notice; no warranty.

Dependencies are MIT/Apache-2.0 licensed (ratatui, crossterm, anyhow), so the
MIT choice is compatible with all of them.
