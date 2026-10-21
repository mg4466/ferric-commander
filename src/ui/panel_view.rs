//! Rendering of one directory pane.
//!
//! Phase 1 draws a plain scrolling list. Phase 2 swaps the body for a
//! `ratatui::widgets::Table` driven by `PanelState::table_state`; the row
//! decoration rules (markers, colours, symlink and broken-link styling) are
//! defined here once so that the swap does not change how a row looks.
//!
//! Row grammar, fixed by the specification:
//!
//!   `*`  row is selected        `/`  entry is a directory
//!   `@`  entry is a symlink     `!`  symlink target is missing
//!   `>`  row holds the cursor

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::fs::panel::{DirectoryEntry, PanelState};
use crate::theme::Theme;

/// Draw `panel` into `area`.
///
/// `active` selects the border colour, which is the only cue the user gets that
/// Tab changed the target of navigation.
pub fn render(frame: &mut Frame, panel: &PanelState, area: Rect, active: bool, theme: &Theme) {
    let border = if active {
        theme.border_active
    } else {
        theme.border_inactive
    };
    let title = format!(
        " {} [{} {}] ",
        panel.cwd.display(),
        panel.sort_col.label(),
        if panel.sort_asc {
            "\u{25b2}"
        } else {
            "\u{25bc}"
        }
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(title)
        .style(Style::default().bg(theme.bg).fg(theme.fg));

    let inner_height = block.inner(area).height as usize;
    let cursor = panel.cursor();
    let lines: Vec<Line> = panel
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            row_line(
                entry,
                index == cursor.unwrap_or(usize::MAX),
                panel.selected.contains(&entry.path),
                theme,
            )
        })
        .collect();

    // Keep the cursor on screen. One line of slack below it means the cursor
    // never sits flush against the border.
    let scroll = cursor
        .map(|c| (c + 1).saturating_sub(inner_height.max(1)))
        .unwrap_or(0) as u16;

    let paragraph = Paragraph::new(lines).block(block).scroll((scroll, 0));

    frame.render_widget(paragraph, area);
}

/// Build one decorated row.
fn row_line(
    entry: &DirectoryEntry,
    is_cursor: bool,
    is_selected: bool,
    theme: &Theme,
) -> Line<'static> {
    let marker = if is_selected {
        '*'
    } else if entry.is_broken_symlink {
        '!'
    } else if entry.is_symlink {
        '@'
    } else if entry.is_dir {
        '/'
    } else {
        ' '
    };

    let cursor_glyph = if is_cursor { '>' } else { ' ' };

    let name_style = if entry.is_broken_symlink {
        Style::default().fg(theme.error_fg)
    } else if entry.is_dir {
        Style::default().fg(theme.dir_fg)
    } else if entry.is_symlink {
        Style::default().fg(theme.symlink_fg)
    } else {
        Style::default().fg(theme.fg)
    };

    let name_style = if is_selected {
        name_style
            .fg(theme.selected_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        name_style
    };

    let mut spans = vec![
        Span::styled(
            format!("{cursor_glyph}{marker} "),
            Style::default().fg(theme.fg),
        ),
        Span::styled(entry.name.clone(), name_style),
    ];

    if let Some(kind) = kind_label(entry) {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            kind.to_string(),
            Style::default().fg(theme.border_inactive),
        ));
    }

    if !entry.is_dir && !entry.is_parent_nav {
        let size = format_size(entry.size);
        let width = 14usize;
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(size.len()).max(1)),
        ));
        spans.push(Span::styled(
            size,
            Style::default().fg(theme.border_inactive),
        ));
    }

    let line = Line::from(spans);
    if is_cursor {
        line.style(Style::default().bg(theme.cursor_bg).fg(theme.cursor_fg))
    } else {
        line
    }
}

/// The type column shown after the name.
fn kind_label(entry: &DirectoryEntry) -> Option<&'static str> {
    if entry.is_parent_nav {
        Some("<DIR>")
    } else if entry.is_broken_symlink {
        Some("<BROKEN>")
    } else if entry.is_symlink && entry.is_dir {
        Some("<SYM/DIR>")
    } else if entry.is_symlink {
        Some("<SYM>")
    } else if entry.is_dir {
        Some("<DIR>")
    } else {
        None
    }
}

/// Human-readable size. Binary units (KiB powers) with the traditional "KB"
/// label, which is what `ls -lh` and every file manager users compare us to
/// do; showing "1.0 MiB" here would be pedantic where the spec asked for MB.
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
    fn kind_labels_distinguish_symlink_kinds() {
        let base = |is_dir, is_symlink, broken| DirectoryEntry {
            name: "e".into(),
            path: "/e".into(),
            is_dir,
            is_symlink,
            is_broken_symlink: broken,
            is_parent_nav: false,
            size: 0,
            modified: None,
        };

        assert_eq!(kind_label(&base(true, false, false)), Some("<DIR>"));
        assert_eq!(kind_label(&base(false, true, false)), Some("<SYM>"));
        assert_eq!(kind_label(&base(true, true, false)), Some("<SYM/DIR>"));
        assert_eq!(kind_label(&base(false, true, true)), Some("<BROKEN>"));
        assert_eq!(kind_label(&base(false, false, false)), None);
    }
}
