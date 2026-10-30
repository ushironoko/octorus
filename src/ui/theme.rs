//! UI palette.
//!
//! #169 and #172 take new colors from `theme.muted`, `theme.accent`,
//! `theme.warning`, `theme.danger`, `theme.success`, `theme.info`,
//! `theme.draft_you`, and `theme.draft_agent`.

use ratatui::style::{Color, Modifier, Style};

use crate::config::{ThemeConfig, ThemePreset};

/// Semantic color plus the modifier that must travel with it.
///
/// There is no getter that returns [`Color`]. A span takes [`Style`] through
/// [`From<Ink>`], so `NO_COLOR`'s `DIM` and `BOLD` cannot be peeled off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ink {
    style: Style,
}

impl Ink {
    pub fn style(self) -> Style {
        self.style
    }

    /// Adds `BOLD` on top of the modifier this ink already has.
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

/// Resolved UI palette. `Copy` so a draw function starts with `let theme = app.theme`.
///
/// `selection` and `focus` are not fields. They are attention and the pane frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub muted: Ink,
    pub accent: Ink,
    pub warning: Ink,
    pub danger: Ink,
    pub success: Ink,
    pub info: Ink,
    pub draft_you: Ink,
    pub draft_agent: Ink,
}

impl Theme {
    /// Infallible. `no_color` wins over the preset and over every color override.
    /// This function does not read the environment.
    pub fn resolve(cfg: &ThemeConfig, no_color: bool) -> Self {
        if no_color {
            return Self {
                muted: Ink::modifier(Modifier::DIM),
                accent: Ink::modifier(Modifier::BOLD),
                warning: Ink::modifier(Modifier::empty()),
                danger: Ink::modifier(Modifier::BOLD),
                success: Ink::modifier(Modifier::empty()),
                info: Ink::modifier(Modifier::empty()),
                draft_you: Ink::modifier(Modifier::empty()),
                draft_agent: Ink::modifier(Modifier::empty()),
            };
        }

        let mut theme = preset(cfg.preset);
        apply_fg(&mut theme.accent, cfg.colors.accent);
        apply_fg(&mut theme.muted, cfg.colors.muted);
        apply_fg(&mut theme.warning, cfg.colors.warning);
        apply_fg(&mut theme.danger, cfg.colors.danger);
        apply_fg(&mut theme.success, cfg.colors.success);
        apply_fg(&mut theme.info, cfg.colors.info);
        apply_fg(&mut theme.draft_you, cfg.colors.draft_you);
        apply_fg(&mut theme.draft_agent, cfg.colors.draft_agent);
        theme
    }

    /// `resolve(&ThemeConfig::default(), false)`.
    pub fn test_default() -> Self {
        Self::resolve(&ThemeConfig::default(), false)
    }

    /// Help tabs. Active is accent plus bold. Inactive is muted.
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
        draft_you: Ink::plain(Color::LightBlue),
        draft_agent: Ink::plain(Color::Magenta),
    };
    match preset {
        ThemePreset::Dark => dark,
        ThemePreset::Light => Theme {
            accent: Ink::plain(Color::Blue),
            info: Ink::plain(Color::Blue),
            draft_you: Ink::plain(Color::Blue),
            ..dark
        },
        ThemePreset::Ansi16 => Theme {
            accent: Ink::plain(Color::Cyan),
            info: Ink::plain(Color::Cyan),
            draft_you: Ink::plain(Color::Blue),
            ..dark
        },
    }
}

fn apply_fg(ink: &mut Ink, color: Option<Color>) {
    if let Some(color) = color {
        *ink = Ink::plain(color);
    }
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

    fn inks(theme: &Theme) -> [(&'static str, Ink); 8] {
        [
            ("accent", theme.accent),
            ("muted", theme.muted),
            ("warning", theme.warning),
            ("danger", theme.danger),
            ("success", theme.success),
            ("info", theme.info),
            ("draft_you", theme.draft_you),
            ("draft_agent", theme.draft_agent),
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

    fn fg(ink: Ink) -> Option<Color> {
        ink.style().fg
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
        assert_eq!(fg(theme.draft_you), Some(Color::LightBlue));
        assert_eq!(fg(theme.draft_agent), Some(Color::Magenta));
        assert_eq!(theme.muted.style().add_modifier, Modifier::empty());
        assert_eq!(theme.muted.style().bg, None);
    }

    #[test]
    fn light_preset_recolors_accent_info_and_draft_you() {
        let cfg = ThemeConfig {
            preset: ThemePreset::Light,
            colors: ThemeColors::default(),
        };
        let theme = Theme::resolve(&cfg, false);
        let dark = Theme::test_default();
        assert_eq!(fg(theme.accent), Some(Color::Blue));
        assert_eq!(fg(theme.info), Some(Color::Blue));
        assert_eq!(fg(theme.draft_you), Some(Color::Blue));
        assert_eq!(theme.muted, dark.muted);
        assert_eq!(theme.warning, dark.warning);
        assert_eq!(theme.danger, dark.danger);
        assert_eq!(theme.success, dark.success);
        assert_eq!(theme.draft_agent, dark.draft_agent);
    }

    #[test]
    fn ansi16_preset_recolors_draft_you_only() {
        let cfg = ThemeConfig {
            preset: ThemePreset::Ansi16,
            colors: ThemeColors::default(),
        };
        let theme = Theme::resolve(&cfg, false);
        let dark = Theme::test_default();
        assert_eq!(fg(theme.accent), Some(Color::Cyan));
        assert_eq!(fg(theme.info), Some(Color::Cyan));
        assert_eq!(fg(theme.draft_you), Some(Color::Blue));
        assert_eq!(theme.muted, dark.muted);
        assert_eq!(theme.warning, dark.warning);
        assert_eq!(theme.danger, dark.danger);
        assert_eq!(theme.success, dark.success);
        assert_eq!(theme.draft_agent, dark.draft_agent);
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
            theme.draft_you,
            theme.draft_agent,
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
        assert_eq!(theme.draft_you.style().add_modifier, Modifier::empty());
        assert_eq!(theme.draft_agent.style().add_modifier, Modifier::empty());
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
            (
                "draft_you",
                sentinel(7),
                ThemeColors {
                    draft_you: Some(sentinel(7)),
                    ..ThemeColors::default()
                },
            ),
            (
                "draft_agent",
                sentinel(8),
                ThemeColors {
                    draft_agent: Some(sentinel(8)),
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
