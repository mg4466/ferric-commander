//! Small display formatters shared by the panes, the status line and the
//! progress dialog.
//!
//! They live here rather than in `ui` because `App` needs the same strings for
//! status messages, and having the state machine depend on the renderer would be
//! backwards.

/// Human-readable size. Binary units (KiB powers) with the traditional "KB"
/// label, which is what `ls -lh` and every file manager users compare us to do;
/// showing "1.0 MiB" here would be pedantic where the specification asked for MB.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    format!("{value:.1} {}", UNITS[unit])
}

/// Shorten a path for a title or a single-line label, keeping the tail.
///
/// The end of a path is the part that identifies the file, so a middle
/// ellipsis ("/very/long/.../name.txt") is the useful truncation. Counted in
/// characters so a UTF-8 path cannot be cut mid-character.
pub fn truncate_path(path: &str, max_chars: usize) -> String {
    let count = path.chars().count();
    if count <= max_chars || max_chars < 5 {
        return path.to_string();
    }

    // Exactly `max_chars` wide: three dots plus the tail.
    let tail: String = path.chars().skip(count - (max_chars - 3)).collect();
    format!("...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_render_in_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(42_100_000), "40.1 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn truncation_keeps_the_tail_and_respects_char_boundaries() {
        assert_eq!(truncate_path("/a/b.txt", 20), "/a/b.txt");
        let cut = truncate_path("/very/long/path/to/file.txt", 16);
        assert_eq!(cut, "...h/to/file.txt");
        assert_eq!(cut.chars().count(), 16, "result fills the budget exactly");
        // Every character is multibyte here; a byte-based cut would panic or
        // produce mojibake.
        let path = "/目录/深层/文件.txt";
        let cut = truncate_path(path, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.starts_with("..."));
    }
}
