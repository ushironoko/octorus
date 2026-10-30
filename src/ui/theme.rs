//! UI palette.
//!
//! #169 and #172 take new colors from the theme fields that still exist:
//! `muted`, `accent`, `warning`, `danger`, `success`, and `info`.
//! Draft colors are added on `Theme` when those issues land.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, HighlightSpacing, List};

use crate::config::{ThemeConfig, ThemePreset};

pub const SELECTION_MARKER_WIDTH: usize = 2;

const FOCUSED_MARKER: &str = "▌ ";
const DIM_MARKER: &str = "▏ ";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ink {
    style: Style,
}

impl Ink {
    pub fn style(self) -> Style {
        self.style
    }

    pub fn bold(self) -> Style {
        self.style.add_modifier(Modifier::BOLD)
    }

    fn plain(color: Color) -> Self {
        Self {
            style: Style::default().fg(color),
        }
    }

    fn modifier(modifier: Modifier) -> Self {
        Self {
            style: Style::default().add_modifier(modifier),
        }
    }
}

impl From<Ink> for Style {
    fn from(ink: Ink) -> Self {
        ink.style
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub muted: Ink,
    pub accent: Ink,
    pub warning: Ink,
    pub danger: Ink,
    pub success: Ink,
    pub info: Ink,
}

impl Theme {
    pub fn resolve(cfg: &ThemeConfig, no_color: bool) -> Self {
        if no_color {
            return Self {
                muted: Ink::modifier(Modifier::DIM),
                accent: Ink::modifier(Modifier::BOLD),
                warning: Ink::modifier(Modifier::empty()),
                danger: Ink::modifier(Modifier::BOLD),
                success: Ink::modifier(Modifier::empty()),
                info: Ink::modifier(Modifier::empty()),
            };
        }

        let mut theme = preset(cfg.preset);
        apply_fg(&mut theme.accent, cfg.colors.accent);
        apply_fg(&mut theme.muted, cfg.colors.muted);
        apply_fg(&mut theme.warning, cfg.colors.warning);
        apply_fg(&mut theme.danger, cfg.colors.danger);
        apply_fg(&mut theme.success, cfg.colors.success);
        apply_fg(&mut theme.info, cfg.colors.info);
        theme
    }

    pub fn test_default() -> Self {
        Self::resolve(&ThemeConfig::default(), false)
    }

    pub fn tab(self, active: bool) -> Style {
        if active {
            self.accent.bold()
        } else {
            self.muted.into()
        }
    }
}

fn preset(preset: ThemePreset) -> Theme {
    let dark = Theme {
        muted: Ink::plain(Color::DarkGray),
        accent: Ink::plain(Color::Cyan),
        warning: Ink::plain(Color::Yellow),
        danger: Ink::plain(Color::Red),
        success: Ink::plain(Color::Green),
        info: Ink::plain(Color::Cyan),
    };
    match preset {
        ThemePreset::Dark | ThemePreset::Ansi16 => dark,
        ThemePreset::Light => Theme {
            accent: Ink::plain(Color::Blue),
            info: Ink::plain(Color::Blue),
            ..dark
        },
    }
}

pub fn no_color_from_value(value: Option<&str>) -> bool {
    matches!(value, Some(text) if !text.is_empty())
}

fn apply_fg(ink: &mut Ink, color: Option<Color>) {
    if let Some(color) = color {
        *ink = Ink::plain(color);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attention {
    Focused,
    Dim,
}

impl Attention {
    pub(crate) fn when(focused: bool) -> Self {
        if focused {
            Self::Focused
        } else {
            Self::Dim
        }
    }

    pub fn rows(self, theme: Theme) -> RowMark {
        match self {
            // The symbol is painted after highlight_style, so it has to carry
            // BOLD itself or the gutter cell loses it.
            Self::Focused => RowMark {
                symbol: FOCUSED_MARKER,
                symbol_style: theme.accent.bold(),
                row_style: Style::new().add_modifier(Modifier::BOLD),
            },
            Self::Dim => RowMark {
                symbol: DIM_MARKER,
                symbol_style: theme.muted.style(),
                row_style: Style::new(),
            },
        }
    }

    pub fn frame(self, theme: Theme) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_type(self.border_type())
            .border_style(self.border_style(theme))
    }

    pub fn border_style(self, theme: Theme) -> Style {
        match self {
            Self::Focused => theme.accent.style(),
            Self::Dim => theme.muted.style(),
        }
    }

    fn border_type(self) -> BorderType {
        match self {
            Self::Focused => BorderType::Thick,
            Self::Dim => BorderType::Plain,
        }
    }

    pub fn has_keys(self) -> bool {
        matches!(self, Self::Focused)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowMark {
    symbol: &'static str,
    symbol_style: Style,
    row_style: Style,
}

impl RowMark {
    pub fn apply<'a>(self, list: List<'a>) -> List<'a> {
        list.highlight_symbol(Line::from(Span::styled(self.symbol, self.symbol_style)))
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(self.row_style)
    }

    pub fn prefix(self, selected: bool) -> Span<'static> {
        if selected {
            Span::styled(self.symbol, self.symbol_style)
        } else {
            Span::raw("  ")
        }
    }

    pub fn body_style(self, selected: bool) -> Style {
        if selected {
            self.row_style
        } else {
            Style::new()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitAttention {
    pub files: Attention,
    pub diff: Attention,
    pub comments: Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffAttention {
    pub diff: Attention,
    pub comments: Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowseAttention {
    pub tree: Attention,
    pub file: Attention,
    pub graph: Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitOpsAttention {
    pub tree: Attention,
    pub commits: Attention,
    pub diff: Attention,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IssueDetailAttention {
    pub body: Attention,
    pub linked_prs: Attention,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemeColors;

    fn with_colors(colors: ThemeColors) -> Theme {
        Theme::resolve(
            &ThemeConfig {
                preset: ThemePreset::Dark,
                colors,
            },
            false,
        )
    }

    fn inks(theme: &Theme) -> [(&'static str, Ink); 6] {
        [
            ("accent", theme.accent),
            ("muted", theme.muted),
            ("warning", theme.warning),
            ("danger", theme.danger),
            ("success", theme.success),
            ("info", theme.info),
        ]
    }

    fn assert_only_field_changed(theme: &Theme, field: &str, sentinel: Color) {
        let base = Theme::test_default();
        for ((name, ink), (_, base_ink)) in inks(theme).into_iter().zip(inks(&base)) {
            if name == field {
                assert_eq!(fg(ink), Some(sentinel), "{name}");
            } else {
                assert_eq!(ink, base_ink, "{name} changed");
            }
        }
    }

    #[test]
    fn no_color_value_is_on_only_when_the_string_is_non_empty() {
        assert!(!no_color_from_value(None));
        assert!(!no_color_from_value(Some("")));
        assert!(no_color_from_value(Some("1")));
        assert!(no_color_from_value(Some(" ")));
    }

    fn fg(ink: Ink) -> Option<Color> {
        ink.style().fg
    }

    #[test]
    fn marker_display_width_equals_selection_marker_width() {
        use unicode_width::UnicodeWidthStr;

        let theme = Theme::test_default();
        let focused = Attention::Focused.rows(theme);
        let dim = Attention::Dim.rows(theme);
        assert_eq!(focused.prefix(true).content.width(), SELECTION_MARKER_WIDTH);
        assert_eq!(
            focused.prefix(false).content.width(),
            SELECTION_MARKER_WIDTH
        );
        assert_eq!(dim.prefix(true).content.width(), SELECTION_MARKER_WIDTH);
        assert_eq!(dim.prefix(false).content.width(), SELECTION_MARKER_WIDTH);
        assert_eq!(focused.prefix(true).content.as_ref(), "▌ ");
        assert_eq!(dim.prefix(true).content.as_ref(), "▏ ");
        assert_eq!(focused.prefix(false).content.as_ref(), "  ");
        assert!(focused
            .prefix(true)
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(!dim.prefix(true).style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(focused.body_style(true).fg, None);
        assert_eq!(focused.body_style(true).bg, None);
        assert!(focused
            .body_style(true)
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(dim.body_style(true).add_modifier, Modifier::empty());
        assert_eq!(focused.body_style(false).add_modifier, Modifier::empty());
    }

    #[test]
    fn test_default_matches_the_dark_preset() {
        let theme = Theme::test_default();
        let resolved = Theme::resolve(&ThemeConfig::default(), false);
        assert_eq!(theme, resolved);
        assert_eq!(fg(theme.muted), Some(Color::DarkGray));
        assert_eq!(fg(theme.accent), Some(Color::Cyan));
        assert_eq!(fg(theme.warning), Some(Color::Yellow));
        assert_eq!(fg(theme.danger), Some(Color::Red));
        assert_eq!(fg(theme.success), Some(Color::Green));
        assert_eq!(fg(theme.info), Some(Color::Cyan));
        assert_eq!(theme.muted.style().add_modifier, Modifier::empty());
        assert_eq!(theme.muted.style().bg, None);
    }

    #[test]
    fn light_preset_recolors_accent_and_info() {
        let cfg = ThemeConfig {
            preset: ThemePreset::Light,
            colors: ThemeColors::default(),
        };
        let theme = Theme::resolve(&cfg, false);
        let dark = Theme::test_default();
        assert_eq!(fg(theme.accent), Some(Color::Blue));
        assert_eq!(fg(theme.info), Some(Color::Blue));
        assert_eq!(theme.muted, dark.muted);
        assert_eq!(theme.warning, dark.warning);
        assert_eq!(theme.danger, dark.danger);
        assert_eq!(theme.success, dark.success);
    }

    #[test]
    fn ansi16_preset_matches_dark() {
        let cfg = ThemeConfig {
            preset: ThemePreset::Ansi16,
            colors: ThemeColors::default(),
        };
        assert_eq!(Theme::resolve(&cfg, false), Theme::test_default());
    }

    #[test]
    fn no_color_drops_foreground_and_ignores_overrides() {
        let cfg = ThemeConfig {
            preset: ThemePreset::Light,
            colors: ThemeColors {
                accent: Some(Color::Magenta),
                danger: Some(Color::Green),
                ..ThemeColors::default()
            },
        };
        let theme = Theme::resolve(&cfg, true);
        for ink in [
            theme.muted,
            theme.accent,
            theme.warning,
            theme.danger,
            theme.success,
            theme.info,
        ] {
            assert_eq!(ink.style().fg, None);
            assert_eq!(ink.style().bg, None);
        }
        assert_eq!(theme.muted.style().add_modifier, Modifier::DIM);
        assert_eq!(theme.accent.style().add_modifier, Modifier::BOLD);
        assert_eq!(theme.danger.style().add_modifier, Modifier::BOLD);
        assert_eq!(theme.warning.style().add_modifier, Modifier::empty());
        assert_eq!(theme.success.style().add_modifier, Modifier::empty());
        assert_eq!(theme.info.style().add_modifier, Modifier::empty());
    }

    #[test]
    fn each_color_override_changes_only_that_field() {
        let sentinel = |n: u8| Color::Indexed(n);
        let cases = [
            (
                "accent",
                sentinel(1),
                ThemeColors {
                    accent: Some(sentinel(1)),
                    ..ThemeColors::default()
                },
            ),
            (
                "muted",
                sentinel(2),
                ThemeColors {
                    muted: Some(sentinel(2)),
                    ..ThemeColors::default()
                },
            ),
            (
                "warning",
                sentinel(3),
                ThemeColors {
                    warning: Some(sentinel(3)),
                    ..ThemeColors::default()
                },
            ),
            (
                "danger",
                sentinel(4),
                ThemeColors {
                    danger: Some(sentinel(4)),
                    ..ThemeColors::default()
                },
            ),
            (
                "success",
                sentinel(5),
                ThemeColors {
                    success: Some(sentinel(5)),
                    ..ThemeColors::default()
                },
            ),
            (
                "info",
                sentinel(6),
                ThemeColors {
                    info: Some(sentinel(6)),
                    ..ThemeColors::default()
                },
            ),
        ];
        for (field, color, colors) in cases {
            assert_only_field_changed(&with_colors(colors), field, color);
        }
        let accent_only = with_colors(ThemeColors {
            accent: Some(Color::Magenta),
            ..ThemeColors::default()
        });
        assert_eq!(fg(accent_only.info), Some(Color::Cyan));
    }
}
