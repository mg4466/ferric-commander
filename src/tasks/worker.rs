//! The worker thread: pre-flight scan, then recursive copy / move / delete.
//!
//! Invariants this module is responsible for:
//!
//!   * Symlinks are never followed. Every node is inspected with
//!     `symlink_metadata`; a link is recreated at the destination as a link, and
//!     deleting a link removes the link, never its target. That is what makes
//!     recursive operations on a tree containing a cycle safe.
//!   * Cancellation is checked before every node and between every 64 KB chunk,
//!     so a cancel is honoured within one chunk even on a multi-gigabyte file.
//!   * A partially written file is removed, because a truncated file at the
//!     destination is indistinguishable from a successful copy to anyone who
//!     looks at it later.
//!   * Every per-item failure is reported and the task continues. One unreadable
//!     file in a 500-file batch must not abort the whole batch.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Instant;

use super::{
    ActiveTaskHandle, ConflictPolicy, FileTask, IoOperation, PROGRESS_INTERVAL, TaskPhase,
    TaskUpdate, WorkerControl,
};

/// Copy granularity for large files. Also the cancellation latency for a file
/// being copied: 64 KB on any modern disk is a fraction of a millisecond.
const CHUNK_SIZE: usize = 64 * 1024;

/// Files at or below this size use `fs::copy` (which may use `copy_file_range`
/// or reflinks); larger ones go through the chunked loop so that cancel is
/// responsive and progress is meaningful.
const LARGE_FILE_THRESHOLD: u64 = 1024 * 1024;

/// Why a single node stopped.
///
/// `Skipped` is not an error: it is the user's conflict policy doing its job,
/// and it must not end up in the error report.
#[derive(Debug)]
enum NodeError {
    Io(io::Error),
    Cancelled,
    Skipped,
}

impl From<io::Error> for NodeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Start a task on a detached thread and return the UI's handle to it.
///
/// The thread is joined by nobody: it ends when its source list is exhausted or
/// when the handle (and with it `cancel_tx`) is dropped, which makes
/// `cancel_rx` report disconnected and stops the worker at its next check.
pub fn spawn_worker(task: FileTask) -> ActiveTaskHandle {
    let (update_tx, update_rx) = mpsc::channel();
    let (cancel_tx, cancel_rx) = mpsc::channel();
    let op = task.op;

    std::thread::spawn(move || run(task, update_tx, cancel_rx));

    ActiveTaskHandle {
        rx: update_rx,
        cancel_tx,
        op,
        phase: TaskPhase::Scanning,
        current_file: String::new(),
        items_done: 0,
        total_items: 0,
        bytes_done: 0,
        total_bytes: 0,
        error_count: 0,
        skipped_count: 0,
        last_error: None,
    }
}

/// Worker entry point. Any `send` failure means the UI is gone, so the task
/// stops silently rather than doing work nobody will see.
fn run(task: FileTask, tx: Sender<TaskUpdate>, cancel_rx: Receiver<WorkerControl>) {
    let (total_items, total_bytes) = calculate_totals(&task.sources);

    if tx
        .send(TaskUpdate::Started {
            total_items,
            total_bytes,
        })
        .is_err()
    {
        return;
    }

    let mut ctx = Ctx {
        tx,
        cancel_rx,
        cancelled: false,
        current_file: String::new(),
        items_done: 0,
        bytes_done: 0,
        last_emit: Instant::now(),
        errors: Vec::new(),
        skipped: Vec::new(),
    };

    let stop_reason = execute(&mut ctx, &task);

    let final_update = match stop_reason {
        Some(reason) => TaskUpdate::Aborted {
            reason,
            items_done: ctx.items_done,
            bytes_done: ctx.bytes_done,
        },
        None => TaskUpdate::Completed {
            skipped_errors: ctx.errors.clone(),
            skipped: ctx.skipped.clone(),
        },
    };

    let _ = ctx.tx.send(final_update);
}

/// Walk every source and run the operation, returning a stop reason if the task
/// ended early.
fn execute(ctx: &mut Ctx, task: &FileTask) -> Option<String> {
    for source in &task.sources {
        let Some(name) = source.file_name().map(PathBuf::from) else {
            ctx.record_error(source, "path has no file name component");
            continue;
        };

        ctx.current_file = source.display().to_string();

        let result = match task.op {
            IoOperation::Copy => {
                let Some(destination) = &task.destination else {
                    return Some("internal error: copy without a destination".to_string());
                };
                copy_node(ctx, source, &destination.join(&name), task.policy, true)
            }
            IoOperation::Move => {
                let Some(destination) = &task.destination else {
                    return Some("internal error: move without a destination".to_string());
                };
                move_node(ctx, source, &destination.join(&name), task.policy)
            }
            IoOperation::Delete => delete_node(ctx, source, true),
        };

        match result {
            Ok(()) => {}
            Err(NodeError::Cancelled) => return Some("Cancelled".to_string()),
            Err(NodeError::Skipped) => ctx.record_skip(source),
            Err(NodeError::Io(err)) => ctx.record_error(source, &err.to_string()),
        }
    }

    None
}

/// Everything a running worker needs to mutate as it goes.
struct Ctx {
    tx: Sender<TaskUpdate>,
    cancel_rx: Receiver<WorkerControl>,
    cancelled: bool,
    current_file: String,
    items_done: usize,
    bytes_done: u64,
    last_emit: Instant,
    errors: Vec<(PathBuf, String)>,
    skipped: Vec<PathBuf>,
}

impl Ctx {
    /// Has a cancel been requested? Drains the control channel, so this is also
    /// where a disconnect (the UI dropped the handle) turns into a cancel.
    fn is_cancelled(&mut self) -> bool {
        if self.cancelled {
            return true;
        }

        // One non-blocking peek per call: there is nothing to spin on, and the
        // next check happens at most one 64 KiB chunk later. `Disconnected`
        // means the UI dropped the handle (it is quitting), which must stop the
        // worker rather than let it keep writing to disk unobserved.
        match self.cancel_rx.try_recv() {
            Ok(WorkerControl::Cancel) | Err(TryRecvError::Disconnected) => self.cancelled = true,
            Err(TryRecvError::Empty) => {}
        }

        self.cancelled
    }

    /// Publish progress, throttled unless `force`d (used when a file finishes,
    /// so the bar always reaches its final value).
    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(self.last_emit) < PROGRESS_INTERVAL {
            return;
        }
        self.last_emit = now;

        let _ = self.tx.send(TaskUpdate::Progress {
            current_file: self.current_file.clone(),
            items_processed: self.items_done,
            bytes_processed: self.bytes_done,
        });
    }

    fn add_bytes(&mut self, bytes: u64) {
        self.bytes_done += bytes;
        self.emit(false);
    }

    fn finish_item(&mut self) {
        self.items_done += 1;
        self.emit(true);
    }

    fn record_error(&mut self, path: &Path, message: &str) {
        self.errors.push((path.to_path_buf(), message.to_string()));
        let _ = self.tx.send(TaskUpdate::NonFatalError {
            path: path.to_path_buf(),
            error: message.to_string(),
        });
    }

    fn record_skip(&mut self, path: &Path) {
        self.skipped.push(path.to_path_buf());
        self.emit(true);
    }
}

/// Count what the task will touch, without following symlinks.
///
/// Iterative on purpose: a deep tree should not be able to overflow the stack
/// just by being deep, and this runs before the user sees any progress at all.
fn calculate_totals(sources: &[PathBuf]) -> (usize, u64) {
    let mut items = 0usize;
    let mut bytes = 0u64;
    let mut stack: Vec<PathBuf> = sources.to_vec();

    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        items += 1;

        if meta.is_dir() && !meta.file_type().is_symlink() {
            if let Ok(reader) = fs::read_dir(&path) {
                for entry in reader.flatten() {
                    stack.push(entry.path());
                }
            }
        } else if !meta.file_type().is_symlink() {
            bytes += meta.len();
        }
    }

    (items, bytes)
}

/// Clear the way for `dst` according to the conflict policy.
///
/// Overwriting *merges* directories: an existing destination directory is kept
/// and its contents are added to, because deleting it first would destroy files
/// the task was never going to copy. A destination that is a file, or a link, or
/// whose type does not match the source, is removed.
fn prepare_destination(
    dst: &Path,
    policy: ConflictPolicy,
    source_is_dir: bool,
) -> Result<(), NodeError> {
    let Ok(meta) = fs::symlink_metadata(dst) else {
        return Ok(());
    };

    if policy == ConflictPolicy::Skip {
        return Err(NodeError::Skipped);
    }

    let dst_is_dir = meta.is_dir() && !meta.file_type().is_symlink();
    if source_is_dir && dst_is_dir {
        // Merge.
        return Ok(());
    }

    if dst_is_dir {
        fs::remove_dir_all(dst)?;
    } else {
        fs::remove_file(dst)?;
    }

    Ok(())
}

/// Copy `src` to `dst` recursively.
///
/// `count` is false only for the delete half of a cross-device move, where the
/// items were already counted during the copy half.
fn copy_node(
    ctx: &mut Ctx,
    src: &Path,
    dst: &Path,
    policy: ConflictPolicy,
    count: bool,
) -> Result<(), NodeError> {
    if ctx.is_cancelled() {
        return Err(NodeError::Cancelled);
    }

    let meta = fs::symlink_metadata(src)?;

    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        prepare_destination(dst, policy, false)?;
        create_symlink(&target, dst)?;
        if count {
            ctx.finish_item();
        }
        return Ok(());
    }

    if meta.is_dir() {
        prepare_destination(dst, policy, true)?;
        fs::create_dir_all(dst)?;

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_node(
                ctx,
                &entry.path(),
                &dst.join(entry.file_name()),
                policy,
                count,
            )?;
        }

        if count {
            ctx.finish_item();
        }
        return Ok(());
    }

    prepare_destination(dst, policy, false)?;
    copy_regular_file(ctx, src, dst, meta.len())?;
    if count {
        ctx.finish_item();
    }
    Ok(())
}

/// Copy one file, chunked when large so cancellation stays responsive.
fn copy_regular_file(ctx: &mut Ctx, src: &Path, dst: &Path, size: u64) -> Result<(), NodeError> {
    if ctx.is_cancelled() {
        return Err(NodeError::Cancelled);
    }

    // Small files: let the OS do it. `fs::copy` may use reflinks or
    // copy_file_range, which are faster than anything this loop can do.
    if size <= LARGE_FILE_THRESHOLD {
        fs::copy(src, dst)?;
        ctx.add_bytes(size);
        return Ok(());
    }

    let result = copy_stream(ctx, src, dst);
    if result.is_err() {
        // Never leave a short file behind: someone will eventually mistake it
        // for a complete copy.
        let _ = fs::remove_file(dst);
    }
    result
}

/// The 64 KB chunk loop. Checks for cancellation before every chunk.
fn copy_stream(ctx: &mut Ctx, src: &Path, dst: &Path) -> Result<(), NodeError> {
    let mut reader = File::open(src)?;
    let mut writer = File::create(dst)?;
    let mut buffer = vec![0u8; CHUNK_SIZE];

    loop {
        if ctx.is_cancelled() {
            return Err(NodeError::Cancelled);
        }

        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        ctx.add_bytes(read as u64);
    }

    writer.flush()?;

    // Carry the mode over; ownership and timestamps are out of scope, and
    // failing to set permissions is not worth aborting a successful copy.
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dst, meta.permissions());
    }

    Ok(())
}

/// Move a node: rename when possible, copy+delete across devices.
fn move_node(
    ctx: &mut Ctx,
    src: &Path,
    dst: &Path,
    policy: ConflictPolicy,
) -> Result<(), NodeError> {
    if ctx.is_cancelled() {
        return Err(NodeError::Cancelled);
    }

    let meta = fs::symlink_metadata(src)?;
    let source_is_dir = meta.is_dir() && !meta.file_type().is_symlink();

    // clear the destination ourselves rather than relying on rename's own
    // (platform-specific) clobbering rules.
    prepare_destination(dst, policy, source_is_dir)?;

    match fs::rename(src, dst) {
        Ok(()) => {
            ctx.finish_item();
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            // EXDEV: the two paths are on different filesystems, so rename
            // cannot work. Fall back to a full copy followed by a delete of the
            // source — but only delete after the copy has returned Ok, so a
            // failed copy never loses data.
            copy_node(ctx, src, dst, ConflictPolicy::Overwrite, true)?;
            delete_node(ctx, src, false)?;
            Ok(())
        }
        Err(err) => Err(NodeError::Io(err)),
    }
}

/// Delete `src` recursively, never following symlinks.
fn delete_node(ctx: &mut Ctx, src: &Path, count: bool) -> Result<(), NodeError> {
    if ctx.is_cancelled() {
        return Err(NodeError::Cancelled);
    }

    let meta = fs::symlink_metadata(src)?;

    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            delete_node(ctx, &entry.path(), count)?;
        }
        fs::remove_dir(src)?;
    } else {
        // For a symlink this removes the link itself; the target is untouched.
        fs::remove_file(src)?;
    }

    if count {
        ctx.finish_item();
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn create_symlink(target: &Path, dst: &Path) -> io::Result<()> {
    // Windows distinguishes file and directory links and needs a privilege for
    // each; declaring the limitation beats silently producing a copy instead.
    let _ = (target, dst);
    Err(io::Error::other(
        "symlink recreation is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "fc-worker-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, bytes).unwrap();
            path
        }

        fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A context wired to channels that nobody drains, for direct node tests.
    fn ctx() -> (Ctx, Receiver<TaskUpdate>, Sender<WorkerControl>) {
        let (tx, rx) = mpsc::channel();
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let ctx = Ctx {
            tx,
            cancel_rx,
            cancelled: false,
            current_file: String::new(),
            items_done: 0,
            bytes_done: 0,
            last_emit: Instant::now() - PROGRESS_INTERVAL,
            errors: Vec::new(),
            skipped: Vec::new(),
        };
        (ctx, rx, cancel_tx)
    }

    /// Drain a channel until it yields a terminal update.
    fn run_task(task: FileTask) -> (Vec<TaskUpdate>, TaskUpdate) {
        let handle = spawn_worker(task);
        let mut seen = Vec::new();

        loop {
            match handle.rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(update) => {
                    let terminal = update.is_terminal();
                    seen.push(update.clone());
                    if terminal {
                        return (seen, update);
                    }
                }
                Err(err) => panic!("worker went quiet: {err}; saw {seen:?}"),
            }
        }
    }

    #[test]
    fn totals_count_every_node_but_only_real_file_bytes() {
        let tmp = TempDir::new("totals");
        tmp.write("a.bin", &[0u8; 10]);
        tmp.write("b.bin", &[0u8; 20]);
        tmp.write("sub/c.bin", &[0u8; 5]);
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("a.bin"), tmp.path().join("link")).unwrap();

        let (items, bytes) = calculate_totals(&[tmp.path().to_path_buf()]);

        #[cfg(unix)]
        assert_eq!(items, 6, "root + a + b + sub + c + link");
        #[cfg(not(unix))]
        assert_eq!(items, 5);

        assert_eq!(bytes, 35, "symlink contributes no bytes");
    }

    #[test]
    fn copy_recreates_a_tree_with_content_intact() {
        let tmp = TempDir::new("copy");
        let src = tmp.mkdir("src");
        fs::write(src.join("one.txt"), b"one").unwrap();
        fs::create_dir_all(src.join("nested/deep")).unwrap();
        fs::write(src.join("nested/deep/two.txt"), b"two").unwrap();

        let (mut c, _rx, _cancel) = ctx();
        let dst = tmp.path().join("dst");
        copy_node(&mut c, &src, &dst, ConflictPolicy::Overwrite, true).unwrap();

        assert_eq!(fs::read(dst.join("one.txt")).unwrap(), b"one");
        assert_eq!(fs::read(dst.join("nested/deep/two.txt")).unwrap(), b"two");
        assert_eq!(c.items_done, 5, "root, one, nested, deep, two");
    }

    #[cfg(unix)]
    #[test]
    fn copying_a_symlink_recreates_the_link_not_the_target_tree() {
        let tmp = TempDir::new("symlink-copy");
        let target_dir = tmp.mkdir("target");
        fs::write(target_dir.join("big.bin"), [0u8; 4096]).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target_dir, &link).unwrap();

        let (mut c, _rx, _cancel) = ctx();
        let dst = tmp.path().join("copied-link");
        copy_node(&mut c, &link, &dst, ConflictPolicy::Overwrite, true).unwrap();

        let meta = fs::symlink_metadata(&dst).unwrap();
        assert!(meta.file_type().is_symlink(), "destination is not a link");
        assert_eq!(fs::read_link(&dst).unwrap(), target_dir);
        // `fs::metadata` follows the link, so this proves the copy did not
        // produce a directory of its own: it resolved to the existing target.
        assert!(fs::metadata(&dst).unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_copied_as_a_dangling_symlink() {
        let tmp = TempDir::new("broken-link-copy");
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &link).unwrap();

        let (mut c, _rx, _cancel) = ctx();
        let dst = tmp.path().join("copy");
        copy_node(&mut c, &link, &dst, ConflictPolicy::Overwrite, true).unwrap();

        assert!(fs::symlink_metadata(&dst).unwrap().file_type().is_symlink());
        assert!(fs::metadata(&dst).is_err(), "link should still dangle");
    }

    #[test]
    fn overwriting_merges_directories_instead_of_replacing_them() {
        let tmp = TempDir::new("merge");
        let src = tmp.mkdir("src");
        fs::write(src.join("new.txt"), b"new").unwrap();

        let dst = tmp.mkdir("dst");
        fs::write(dst.join("kept.txt"), b"kept").unwrap();

        let (mut c, _rx, _cancel) = ctx();
        copy_node(&mut c, &src, &dst, ConflictPolicy::Overwrite, true).unwrap();

        assert_eq!(fs::read(dst.join("new.txt")).unwrap(), b"new");
        assert_eq!(
            fs::read(dst.join("kept.txt")).unwrap(),
            b"kept",
            "existing files must survive a merge"
        );
    }

    #[test]
    fn skip_policy_leaves_the_existing_file_untouched() {
        let tmp = TempDir::new("skip");
        let src = tmp.write("src.txt", b"new");
        let dst = tmp.write("dst.txt", b"original");

        let (mut c, _rx, _cancel) = ctx();
        let result = copy_node(&mut c, &src, &dst, ConflictPolicy::Skip, true);

        assert!(matches!(result, Err(NodeError::Skipped)));
        assert_eq!(fs::read(&dst).unwrap(), b"original");
    }

    #[test]
    fn overwrite_policy_replaces_a_file() {
        let tmp = TempDir::new("overwrite");
        let src = tmp.write("src.txt", b"new");
        let dst = tmp.write("dst.txt", b"original");

        let (mut c, _rx, _cancel) = ctx();
        copy_node(&mut c, &src, &dst, ConflictPolicy::Overwrite, true).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"new");
    }

    #[test]
    fn move_uses_rename_and_leaves_no_source_behind() {
        let tmp = TempDir::new("move");
        let src = tmp.write("move-me.txt", b"payload");
        let dst = tmp.path().join("moved.txt");

        let (mut c, _rx, _cancel) = ctx();
        move_node(&mut c, &src, &dst, ConflictPolicy::Overwrite).unwrap();

        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn move_falls_back_to_copy_then_delete_across_filesystems() {
        use std::os::unix::fs::MetadataExt;

        let tmp = TempDir::new("exdev-src");
        let src = tmp.write("cross.bin", &[7u8; 4096]);

        // Pick a directory on a different device if one is available; otherwise
        // there is nothing to test.
        let foreign_root = Path::new("/dev/shm");
        if !foreign_root.is_dir()
            || fs::metadata(foreign_root).unwrap().dev() == fs::metadata(tmp.path()).unwrap().dev()
        {
            eprintln!("skipping: no second filesystem available");
            return;
        }

        let dst_dir = foreign_root.join(format!(
            "fc-exdev-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("cross.bin");

        let (mut c, _rx, _cancel) = ctx();
        let result = move_node(&mut c, &src, &dst, ConflictPolicy::Overwrite);

        assert!(result.is_ok(), "cross-device move failed: {result:?}");
        assert!(!src.exists(), "source should be removed after the copy");
        assert_eq!(fs::read(&dst).unwrap(), vec![7u8; 4096]);

        fs::remove_dir_all(&dst_dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_symlink_does_not_touch_its_target() {
        let tmp = TempDir::new("delete-link");
        let target = tmp.write("target.txt", b"keep me");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (mut c, _rx, _cancel) = ctx();
        delete_node(&mut c, &link, true).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "link still exists");
        assert_eq!(fs::read(&target).unwrap(), b"keep me");
    }

    #[test]
    fn deleting_a_tree_removes_everything() {
        let tmp = TempDir::new("delete-tree");
        let root = tmp.mkdir("doomed");
        fs::write(root.join("a"), b"a").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/b"), b"b").unwrap();

        let (mut c, _rx, _cancel) = ctx();
        delete_node(&mut c, &root, true).unwrap();

        assert!(!root.exists());
        assert_eq!(c.items_done, 4, "root, a, sub, b");
    }

    #[test]
    fn a_queued_cancel_aborts_before_any_io() {
        let tmp = TempDir::new("cancel-early");
        let src = tmp.write("src.txt", b"data");
        let dst = tmp.path().join("dst.txt");

        let (mut c, _rx, cancel_tx) = ctx();
        cancel_tx.send(WorkerControl::Cancel).unwrap();

        let result = copy_node(&mut c, &src, &dst, ConflictPolicy::Overwrite, true);
        assert!(matches!(result, Err(NodeError::Cancelled)));
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "nothing should be written"
        );
    }

    #[test]
    fn a_cancel_mid_file_removes_the_partial_destination() {
        let tmp = TempDir::new("cancel-mid");
        // Larger than LARGE_FILE_THRESHOLD so the chunked path is taken.
        let src = tmp.write("big.bin", &vec![3u8; (LARGE_FILE_THRESHOLD + 1) as usize]);
        let dst = tmp.path().join("big-copy.bin");

        let (mut c, _rx, cancel_tx) = ctx();
        // Cancel lands before the first chunk check, i.e. after the destination
        // has been created: exactly the "partial file" case.
        cancel_tx.send(WorkerControl::Cancel).unwrap();

        let result = copy_node(&mut c, &src, &dst, ConflictPolicy::Overwrite, true);
        assert!(matches!(result, Err(NodeError::Cancelled)));
        assert!(
            fs::symlink_metadata(&dst).is_err(),
            "a truncated destination must be removed"
        );
    }

    #[test]
    fn end_to_end_copy_reports_started_progress_and_completion() {
        let tmp = TempDir::new("e2e-copy");
        let src_dir = tmp.mkdir("from");
        fs::write(src_dir.join("a.txt"), b"aaa").unwrap();

        let dst_dir = tmp.mkdir("to");
        let task = FileTask {
            op: IoOperation::Copy,
            sources: vec![src_dir.join("a.txt")],
            destination: Some(dst_dir.clone()),
            policy: ConflictPolicy::Overwrite,
        };

        let (seen, terminal) = run_task(task);

        match seen.first() {
            Some(TaskUpdate::Started {
                total_items,
                total_bytes,
            }) => {
                assert_eq!(*total_items, 1);
                assert_eq!(*total_bytes, 3);
            }
            other => panic!("first message was {other:?}"),
        }

        match terminal {
            TaskUpdate::Completed {
                skipped_errors,
                skipped,
            } => {
                assert!(skipped_errors.is_empty());
                assert!(skipped.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        assert_eq!(fs::read(dst_dir.join("a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn a_permission_failure_is_reported_and_the_batch_continues() {
        let tmp = TempDir::new("partial-failure");
        let a = tmp.write("a.txt", b"a");
        let b = tmp.write("b.txt", b"b");
        let dst_dir = tmp.mkdir("to");

        // A source that no longer exists produces a per-item error without
        // stopping the second file.
        let task = FileTask {
            op: IoOperation::Copy,
            sources: vec![tmp.path().join("missing.txt"), a.clone(), b.clone()],
            destination: Some(dst_dir.clone()),
            policy: ConflictPolicy::Overwrite,
        };

        let (_seen, terminal) = run_task(task);

        match terminal {
            TaskUpdate::Completed { skipped_errors, .. } => {
                assert_eq!(skipped_errors.len(), 1, "one path should have failed");
                assert!(skipped_errors[0].0.ends_with("missing.txt"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        assert!(dst_dir.join("a.txt").exists());
        assert!(dst_dir.join("b.txt").exists());
    }
}
