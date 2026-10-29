use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

pub(crate) fn find_column(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
    let width = buf.area().width;
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    for start in 0..width {
        let mut x = start;
        let mut matched = 0;
        let mut found = true;
        while matched < needle.len() {
            if x >= width {
                found = false;
                break;
            }
            let mut chars = buf[(x, y)].symbol().chars();
            let Some(ch) = chars.next() else {
                found = false;
                break;
            };
            if chars.next().is_some() || ch != needle[matched] {
                found = false;
                break;
            }
            matched += 1;
            x += 1;
        }
        if found {
            return Some(start);
        }
    }
    None
}

pub(crate) fn row_text(buf: &Buffer, y: u16) -> String {
    let mut line = String::new();
    let mut x = 0;
    while x < buf.area().width {
        let sym = buf[(x, y)].symbol();
        let width = u16::try_from(sym.width()).unwrap_or(1).max(1);
        line.push_str(sym);
        x = x.saturating_add(width);
    }
    line.trim_end().to_string()
}

pub(crate) fn screen_text(buf: &Buffer) -> String {
    (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}
