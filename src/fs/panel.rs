//! Directory listing state for one pane.
//!
//! Invariant: `entries` is always sorted by [`PanelState::sort`] before it is
//! handed to the renderer, and index 0 is the synthetic ".." entry whenever the
//! panel has a parent. Code that mutates `entries` must call `sort` afterwards;
//! `refresh` is the only supported way in.
//!
//! This module is strictly synchronous and single-threaded. It is called from
//! the UI thread only, which is why it may touch the filesystem directly: a
//! directory read of a local filesystem is microseconds, and the operations
//! that can genuinely block (copying gigabytes) live in `tasks`.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use ratatui::widgets::TableState;

/// Display name of the synthetic "go to parent" row.
pub const PARENT_ENTRY_NAME: &str = "..";

/// One row in a panel.
///
/// `is_dir` describes the *effective* target of a symlink (so a link to a
/// directory sorts and navigates like a directory), while `is_symlink` and
/// `is_broken_symlink` describe the link itself. Keeping all three means the
/// renderer never has to stat anything.
#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_broken_symlink: bool,
    /// True only for the synthetic ".." row, which cannot be selected.
    pub is_parent_nav: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl DirectoryEntry {
    /// The synthetic parent row for `cwd`.
    fn parent_nav(cwd: &Path) -> Self {
        let path = cwd
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.to_path_buf());

        Self {
            name: PARENT_ENTRY_NAME.to_string(),
            path,
            is_dir: true,
            is_symlink: false,
            is_broken_symlink: false,
            is_parent_nav: true,
            size: 0,
            modified: None,
        }
    }

    /// Build an entry from a directory iterator item.
    ///
    /// Symlinks are inspected with `symlink_metadata` first so that the link's
    /// own type is known, then `metadata` is used only to learn what the target
    /// is. A failure of the second call means the link dangles, which is a
    /// display state rather than an error.
    fn from_dir_entry(entry: fs::DirEntry) -> io::Result<Self> {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let link_md = fs::symlink_metadata(&path)?;
        let is_symlink = link_md.file_type().is_symlink();

        let (is_dir, is_broken_symlink) = if is_symlink {
            match fs::metadata(&path) {
                Ok(target_md) => (target_md.is_dir(), false),
                Err(_) => (false, true),
            }
        } else {
            (link_md.is_dir(), false)
        };

        Ok(Self {
            name,
            path,
            is_dir,
            is_symlink,
            is_broken_symlink,
            is_parent_nav: false,
            // A directory's own metadata length is meaningless to a user, so it
            // stays 0; recursive sizing is deliberately out of scope.
            size: if is_dir { 0 } else { link_md.len() },
            modified: link_md.modified().ok(),
        })
    }
}

/// Which field the listing is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Size,
    Modified,
}

impl SortColumn {
    /// Label used in the panel title, e.g. "Size".
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Modified => "Modified",
        }
    }
}

/// All mutable state for one pane: where we are, what is there, what is picked.
#[derive(Debug)]
pub struct PanelState {
    pub cwd: PathBuf,
    pub entries: Vec<DirectoryEntry>,
    pub table_state: TableState,
    /// Absolute paths, not row indices: the selection must survive a refresh
    /// that reorders or removes rows, and indices cannot.
    pub selected: HashSet<PathBuf>,
    pub sort_col: SortColumn,
    pub sort_asc: bool,
}

impl PanelState {
    /// Create a panel rooted at `path` and read it immediately.
    ///
    /// Returns an error only if the directory cannot be listed at all; that is
    /// a startup-fatal condition which `App::new` surfaces to the caller.
    pub fn new(path: PathBuf) -> Result<Self> {
        let mut panel = Self {
            cwd: path,
            entries: Vec::new(),
            table_state: TableState::default(),
            selected: HashSet::new(),
            sort_col: SortColumn::Name,
            sort_asc: true,
        };
        panel.refresh()?;
        panel.table_state.select(Some(0));
        Ok(panel)
    }

    /// Re-read `cwd` from disk, preserving the cursor and selection by path.
    ///
    /// Everything the UI shows comes from here, so this is also where stale
    /// selection entries and out-of-range cursors are repaired.
    pub fn refresh(&mut self) -> Result<()> {
        let focused = self.selected_entry().map(|e| e.path.clone());

        let mut entries = Vec::new();
        if self.cwd.parent().is_some() {
            entries.push(DirectoryEntry::parent_nav(&self.cwd));
        }

        let reader = fs::read_dir(&self.cwd)
            .with_context(|| format!("cannot list {}", self.cwd.display()))?;

        for item in reader {
            // A single unreadable child (deleted between readdir and stat, or a
            // permission hole) must not abort the whole listing.
            if let Ok(entry) = item.and_then(DirectoryEntry::from_dir_entry) {
                entries.push(entry);
            }
        }

        self.entries = entries;
        self.sort();
        self.retain_valid_selection();
        self.focus_path(focused.as_deref());
        Ok(())
    }

    /// Order the listing: ".." pinned, then directories, then the sort column.
    ///
    /// Directory-first grouping is intentional and not affected by `sort_asc`;
    /// the direction toggle only reverses the comparison within a group, which
    /// is what users of dual-pane managers expect.
    pub fn sort(&mut self) {
        let head = usize::from(self.entries.first().is_some_and(|e| e.is_parent_nav));
        let col = self.sort_col;
        let asc = self.sort_asc;

        let body = &mut self.entries[head..];
        body.sort_by(|a, b| {
            // Case-insensitive for names so "README" and "app" interleave the
            // way a human expects; falls back to the raw name as a tiebreak to
            // keep the order total and therefore stable.
            let by_col = match col {
                SortColumn::Name => a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.name.cmp(&b.name)),
                SortColumn::Size => a.size.cmp(&b.size),
                SortColumn::Modified => a.modified.cmp(&b.modified),
            };

            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| if asc { by_col } else { by_col.reverse() })
        });
    }

    /// Advance through the F2 cycle: Name asc -> Name desc -> Size desc -> Modified desc.
    pub fn cycle_sort(&mut self) {
        (self.sort_col, self.sort_asc) = match (self.sort_col, self.sort_asc) {
            (SortColumn::Name, true) => (SortColumn::Name, false),
            (SortColumn::Name, false) => (SortColumn::Size, false),
            (SortColumn::Size, _) => (SortColumn::Modified, false),
            (SortColumn::Modified, _) => (SortColumn::Name, true),
        };
        self.sort();
        self.focus_path(self.selected_entry().map(|e| e.path.clone()).as_deref());
    }

    /// True when the panel has nothing but the ".." row (or nothing at all).
    #[allow(dead_code)] // Used by the empty-listing hint in Phase 2.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| e.is_parent_nav)
    }

    /// Row index of the cursor.
    pub fn cursor(&self) -> Option<usize> {
        self.table_state.selected()
    }

    /// Entry under the cursor.
    pub fn selected_entry(&self) -> Option<&DirectoryEntry> {
        self.table_state
            .selected()
            .and_then(|index| self.entries.get(index))
    }

    /// Move the cursor one row up, clamped at the top.
    ///
    /// Clamping rather than wrapping is the convention every file manager uses;
    /// wrapping here would make holding j/k disorienting.
    pub fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let next = self.cursor().unwrap_or(0).saturating_sub(1);
        self.table_state.select(Some(next));
    }

    /// Move the cursor one row down, clamped at the bottom.
    pub fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let next = self.cursor().map_or(0, |c| (c + 1).min(last));
        self.table_state.select(Some(next));
    }

    /// Toggle selection on the cursor row and step down.
    ///
    /// ".." is a navigation affordance, not a file, so it refuses selection.
    /// The cursor advance mirrors midnight-commander behaviour and makes
    /// selecting a run of files a matter of holding Space.
    pub fn toggle_select(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.is_parent_nav {
            return;
        }

        let path = entry.path.clone();
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
        self.move_down();
    }

    /// Invert the selection over every selectable row.
    pub fn invert_selection(&mut self) {
        for entry in self.entries.iter().filter(|e| !e.is_parent_nav) {
            let path = entry.path.clone();
            if !self.selected.remove(&path) {
                self.selected.insert(path);
            }
        }
    }

    /// Enter a directory under the cursor. Returns true if the panel moved.
    ///
    /// The target is listed *before* `cwd` changes, so navigating into an
    /// unreadable directory returns an error and leaves the panel exactly where
    /// it was. Directory symlinks are followed here by design: navigation is
    /// allowed to traverse links even though recursive operations never do.
    pub fn enter_directory(&mut self) -> Result<bool> {
        let Some(entry) = self.selected_entry() else {
            return Ok(false);
        };
        if !entry.is_dir || entry.is_parent_nav || entry.is_broken_symlink {
            return Ok(false);
        }

        let target = entry.path.clone();
        fs::read_dir(&target).with_context(|| format!("cannot open {}", target.display()))?;

        self.cwd = target;
        self.selected.clear();
        self.refresh()?;
        self.table_state.select(Some(0));
        Ok(true)
    }

    /// Move to the parent directory, leaving the cursor on the directory we
    /// came out of so the user does not lose their place.
    pub fn go_parent(&mut self) -> Result<bool> {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return Ok(false);
        };

        fs::read_dir(&parent).with_context(|| format!("cannot open {}", parent.display()))?;

        let previous = self.cwd.clone();
        self.cwd = parent;
        self.selected.clear();
        self.refresh()?;
        self.focus_path(Some(&previous));
        Ok(true)
    }

    /// The paths an operation should act on.
    ///
    /// An explicit selection wins; otherwise the single row under the cursor is
    /// the implicit operand. ".." is never an operand. The order follows the
    /// listing rather than the hash set, so progress reporting is deterministic
    /// and testable.
    #[allow(dead_code)] // The operand source for every Phase 4 operation.
    pub fn operated_paths(&self) -> Vec<PathBuf> {
        let explicit: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| !e.is_parent_nav && self.selected.contains(&e.path))
            .map(|e| e.path.clone())
            .collect();

        if !explicit.is_empty() {
            return explicit;
        }

        self.selected_entry()
            .filter(|e| !e.is_parent_nav)
            .map(|e| vec![e.path.clone()])
            .unwrap_or_default()
    }

    /// Sum of the sizes of the currently selected entries, for the status bar.
    pub fn selected_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| self.selected.contains(&e.path))
            .map(|e| e.size)
            .sum()
    }

    /// Drop selection entries whose files no longer exist in the listing.
    fn retain_valid_selection(&mut self) {
        self.selected
            .retain(|path| self.entries.iter().any(|e| &e.path == path));
    }

    /// Put the cursor on `path` if it is present, else clamp it into range.
    fn focus_path(&mut self, path: Option<&Path>) {
        if let Some(target) = path
            && let Some(index) = self.entries.iter().position(|e| e.path == target)
        {
            self.table_state.select(Some(index));
            return;
        }

        let index = match self.table_state.selected() {
            Some(_) if self.entries.is_empty() => None,
            Some(current) => Some(current.min(self.entries.len() - 1)),
            None if self.entries.is_empty() => None,
            None => Some(0),
        };
        self.table_state.select(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway directory under the system temp dir. `Drop` removes it, so a
    /// failing assertion cannot leave litter behind.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let unique = format!(
                "fc-panel-test-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn touch(&self, name: &str, bytes: usize) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, vec![b'x'; bytes]).expect("write temp file");
            path
        }

        fn mkdir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).expect("create temp subdir");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parent_row_is_pinned_and_not_selectable() {
        let tmp = TempDir::new();
        tmp.touch("a.txt", 1);
        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();

        assert!(panel.entries[0].is_parent_nav);
        assert_eq!(panel.entries[0].name, PARENT_ENTRY_NAME);

        panel.table_state.select(Some(0));
        panel.toggle_select();
        assert!(panel.selected.is_empty());
        assert!(panel.operated_paths().is_empty());
    }

    #[test]
    fn directories_sort_above_files_in_both_directions() {
        let tmp = TempDir::new();
        tmp.touch("aaa.txt", 10);
        tmp.mkdir("zzz_dir");

        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();
        for asc in [true, false] {
            panel.sort_asc = asc;
            panel.sort();
            let body: Vec<_> = panel.entries.iter().filter(|e| !e.is_parent_nav).collect();
            assert!(body[0].is_dir, "directory must lead with asc={asc}");
            assert!(!body[1].is_dir);
        }
    }

    #[test]
    fn sort_cycle_matches_spec_order() {
        let tmp = TempDir::new();
        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();

        let seen = |p: &PanelState| (p.sort_col, p.sort_asc);
        assert_eq!(seen(&panel), (SortColumn::Name, true));
        panel.cycle_sort();
        assert_eq!(seen(&panel), (SortColumn::Name, false));
        panel.cycle_sort();
        assert_eq!(seen(&panel), (SortColumn::Size, false));
        panel.cycle_sort();
        assert_eq!(seen(&panel), (SortColumn::Modified, false));
        panel.cycle_sort();
        assert_eq!(seen(&panel), (SortColumn::Name, true));
    }

    #[test]
    fn size_sort_is_numeric_not_lexical() {
        let tmp = TempDir::new();
        tmp.touch("small.bin", 5);
        tmp.touch("big.bin", 5_000);

        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();
        panel.sort_col = SortColumn::Size;
        panel.sort_asc = false;
        panel.sort();

        let names: Vec<_> = panel
            .entries
            .iter()
            .filter(|e| !e.is_parent_nav)
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, ["big.bin", "small.bin"]);
    }

    #[test]
    fn operated_paths_falls_back_to_cursor_then_selection() {
        let tmp = TempDir::new();
        tmp.touch("one.txt", 1);
        tmp.touch("two.txt", 2);
        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();

        panel.table_state.select(Some(1));
        let single = panel.operated_paths();
        assert_eq!(single.len(), 1);

        panel.selected.insert(tmp.path().join("one.txt"));
        panel.selected.insert(tmp.path().join("two.txt"));
        let both = panel.operated_paths();
        assert_eq!(both.len(), 2);
    }

    #[test]
    fn selection_is_pruned_when_a_file_disappears() {
        let tmp = TempDir::new();
        let gone = tmp.touch("gone.txt", 1);
        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();

        panel.selected.insert(gone.clone());
        fs::remove_file(&gone).unwrap();
        panel.refresh().unwrap();

        assert!(panel.selected.is_empty());
        assert!(panel.cursor().unwrap() < panel.entries.len());
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_is_flagged_not_fatal() {
        let tmp = TempDir::new();
        std::os::unix::fs::symlink(tmp.path().join("nope"), tmp.path().join("dangling")).unwrap();

        let panel = PanelState::new(tmp.path().to_path_buf()).unwrap();
        let entry = panel
            .entries
            .iter()
            .find(|e| e.name == "dangling")
            .expect("dangling entry present");

        assert!(entry.is_symlink);
        assert!(entry.is_broken_symlink);
        assert!(!entry.is_dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_directory_navigates_like_a_directory() {
        let tmp = TempDir::new();
        let real = tmp.mkdir("real");
        fs::write(real.join("inner.txt"), b"hi").unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();

        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();
        let index = panel.entries.iter().position(|e| e.name == "link").unwrap();
        assert!(panel.entries[index].is_dir);

        let link = tmp.path().join("link");
        panel.table_state.select(Some(index));
        assert!(panel.enter_directory().unwrap());

        // The pane keeps the *link* path as its cwd, exactly like `cd link` in a
        // shell: that keeps "go to parent" pointing back where the user came
        // from, instead of dumping them in the target's real parent.
        assert_eq!(panel.cwd, link);
        assert_eq!(panel.entries.iter().filter(|e| !e.is_parent_nav).count(), 1);
        assert!(panel.entries.iter().any(|e| e.name == "inner.txt"));
        let _ = real;
    }

    #[test]
    fn unreadable_target_leaves_cwd_untouched() {
        let tmp = TempDir::new();
        tmp.mkdir("child");
        let mut panel = PanelState::new(tmp.path().to_path_buf()).unwrap();

        // Delete the child after the listing was taken, then try to enter it.
        fs::remove_dir(tmp.path().join("child")).unwrap();
        let index = panel
            .entries
            .iter()
            .position(|e| e.name == "child")
            .unwrap();
        panel.table_state.select(Some(index));

        assert!(panel.enter_directory().is_err());
        assert_eq!(panel.cwd, tmp.path());
    }
}
