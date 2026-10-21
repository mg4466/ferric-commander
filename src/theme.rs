//! Colour palettes.
//!
//! A theme is a flat, `Copy` struct of named colour roles rather than a widget
//! style per widget kind. Widgets ask for the role they mean ("a directory",
//! "an error"), so adding a palette never requires touching the renderer, and
//! adding a *role* shows up as a compile error in every palette at once.
//!
//! Note that `bg` is used as the terminal background for the whole frame; the
//! UI draws its own blocks on top of it because terminal themes vary too much
//! to rely on the emulator's defaults.

use ratatui::style::Color;

/// Which palette is active. Deliberately small and `Copy` so it can live
/// directly in `App` and be cycled with a single assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeType {
    CatppuccinMocha,
    Dracula,
    TokyoNight,
}

impl ThemeType {
    /// Next palette in the F1 cycle.
    ///
    /// Written as an exhaustive match rather than arithmetic on an index so
    /// that adding a palette produces a compile error here instead of
    /// silently changing the cycle order.
    pub fn next(self) -> Self {
        match self {
            Self::CatppuccinMocha => Self::Dracula,
            Self::Dracula => Self::TokyoNight,
            Self::TokyoNight => Self::CatppuccinMocha,
        }
    }

    /// Resolve this selection into concrete RGB values.
    pub fn palette(self) -> Theme {
        match self {
            Self::CatppuccinMocha => Theme {
                name: "Catppuccin Mocha",
                bg: Color::Rgb(30, 30, 46),
                fg: Color::Rgb(205, 214, 244),
                border_inactive: Color::Rgb(88, 91, 112),
                border_active: Color::Rgb(137, 180, 250),
                cursor_bg: Color::Rgb(69, 71, 90),
                cursor_fg: Color::Rgb(205, 214, 244),
                selected_fg: Color::Rgb(249, 226, 175),
                dir_fg: Color::Rgb(137, 180, 250),
                symlink_fg: Color::Rgb(148, 226, 213),
                error_fg: Color::Rgb(243, 139, 168),
                fkey_bg: Color::Rgb(24, 24, 37),
                fkey_fg: Color::Rgb(166, 227, 161),
            },
            Self::Dracula => Theme {
                name: "Dracula",
                bg: Color::Rgb(40, 42, 54),
                fg: Color::Rgb(248, 248, 242),
                border_inactive: Color::Rgb(98, 114, 164),
                border_active: Color::Rgb(189, 147, 249),
                cursor_bg: Color::Rgb(68, 71, 90),
                cursor_fg: Color::Rgb(248, 248, 242),
                selected_fg: Color::Rgb(241, 250, 140),
                dir_fg: Color::Rgb(139, 233, 253),
                symlink_fg: Color::Rgb(255, 121, 198),
                error_fg: Color::Rgb(255, 85, 85),
                fkey_bg: Color::Rgb(33, 34, 44),
                fkey_fg: Color::Rgb(80, 250, 123),
            },
            Self::TokyoNight => Theme {
                name: "Tokyo Night",
                bg: Color::Rgb(26, 27, 38),
                fg: Color::Rgb(192, 202, 245),
                border_inactive: Color::Rgb(65, 72, 104),
                border_active: Color::Rgb(122, 162, 247),
                cursor_bg: Color::Rgb(41, 46, 66),
                cursor_fg: Color::Rgb(192, 202, 245),
                selected_fg: Color::Rgb(224, 175, 104),
                dir_fg: Color::Rgb(125, 207, 255),
                symlink_fg: Color::Rgb(187, 154, 247),
                error_fg: Color::Rgb(247, 118, 142),
                fkey_bg: Color::Rgb(22, 22, 30),
                fkey_fg: Color::Rgb(158, 206, 106),
            },
        }
    }
}

/// One resolved palette.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Human-readable name, shown in the status bar.
    pub name: &'static str,
    pub bg: Color,
    pub fg: Color,
    pub border_inactive: Color,
    pub border_active: Color,
    pub cursor_bg: Color,
    pub cursor_fg: Color,
    pub selected_fg: Color,
    pub dir_fg: Color,
    pub symlink_fg: Color,
    pub error_fg: Color,
    pub fkey_bg: Color,
    pub fkey_fg: Color,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_cycles_back_to_itself() {
        for start in [
            ThemeType::CatppuccinMocha,
            ThemeType::Dracula,
            ThemeType::TokyoNight,
        ] {
            assert_eq!(start.next().next().next(), start);
            assert_ne!(start.next(), start);
        }
    }

    #[test]
    fn palette_names_are_unique() {
        let names: Vec<_> = [
            ThemeType::CatppuccinMocha,
            ThemeType::Dracula,
            ThemeType::TokyoNight,
        ]
        .iter()
        .map(|t| t.palette().name)
        .collect();

        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
