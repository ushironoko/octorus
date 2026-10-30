//! Pane frame. Compact header and footer bars are not here.

use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use super::theme::{Attention, Theme};

/// How a block gets its title. The enum is the uppercase rule.
pub enum PaneTitle<'a> {
    /// `label` is the fixed word. `detail` is the dynamic tail.
    /// Focused uppercases `label` only. An empty `label` skips that and
    /// still takes `▶ ` while the pane has the keys.
    Labeled { label: &'a str, detail: &'a str },
    /// Title bytes are copied through. No arrow and no case change.
    AsIs(&'a str),
}

/// Boxed pane frame. The title is owned, so a `format!` temporary can end
/// on the call.
pub fn pane_block(theme: Theme, title: PaneTitle<'_>, attention: Attention) -> Block<'static> {
    let text = title_text(title, attention);
    let style = if attention.has_keys() {
        theme.accent.bold()
    } else {
        theme.muted.style()
    };
    attention
        .frame(theme)
        .title(Line::from(Span::styled(text, style)))
}

fn title_text(title: PaneTitle<'_>, attention: Attention) -> String {
    match title {
        PaneTitle::AsIs(text) => text.to_string(),
        PaneTitle::Labeled { label, detail } => {
            let label = if attention.has_keys() {
                label.to_uppercase()
            } else {
                label.to_string()
            };
            let body = match (label.is_empty(), detail.is_empty()) {
                (true, _) => detail.to_string(),
                (_, true) => label,
                (false, false) => format!("{label} {detail}"),
            };
            if attention.has_keys() {
                format!("▶ {body}")
            } else {
                body
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_buffer::screen_text;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn block_text(block: Block<'static>) -> String {
        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(block, frame.area());
            })
            .unwrap();
        screen_text(terminal.backend().buffer())
    }

    #[test]
    fn focused_label_is_uppercased_and_detail_is_not() {
        let text = block_text(pane_block(
            Theme::test_default(),
            PaneTitle::Labeled {
                label: "Files",
                detail: "(3) [tree]",
            },
            Attention::Focused,
        ));
        assert!(
            text.contains("▶ FILES (3) [tree]"),
            "focused title missing from:\n{text}"
        );
    }

    #[test]
    fn dim_label_keeps_its_case_and_has_no_arrow() {
        let text = block_text(pane_block(
            Theme::test_default(),
            PaneTitle::Labeled {
                label: "Files",
                detail: "(3) [tree]",
            },
            Attention::Dim,
        ));
        assert!(
            text.contains("Files (3) [tree]"),
            "dim title missing from:\n{text}"
        );
        assert!(!text.contains('▶'), "dim title drew an arrow:\n{text}");
    }

    #[test]
    fn empty_label_keeps_the_detail_and_takes_the_arrow() {
        let text = block_text(pane_block(
            Theme::test_default(),
            PaneTitle::Labeled {
                label: "",
                detail: "src/app.rs (1/4)",
            },
            Attention::Focused,
        ));
        assert!(
            text.contains("▶ src/app.rs (1/4)"),
            "path title missing from:\n{text}"
        );
        assert!(!text.contains("SRC/APP.RS"), "path was uppercased:\n{text}");
    }

    #[test]
    fn as_is_title_has_no_arrow() {
        let text = block_text(pane_block(
            Theme::test_default(),
            PaneTitle::AsIs("Git Status"),
            Attention::Focused,
        ));
        assert!(text.contains("Git Status"), "{text}");
        assert!(!text.contains('▶'), "{text}");
    }
}
