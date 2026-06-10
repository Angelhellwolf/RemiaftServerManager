use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

pub(super) fn wrap_console_lines(lines: &[String], width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for line in lines {
        wrap_console_line(line, width, &mut wrapped);
    }
    wrapped
}

fn wrap_console_line(line: &str, width: usize, output: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width = 0;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            push_ansi_sequence(ch, &mut chars, &mut current);
            continue;
        }

        if ch == '\r' {
            current.clear();
            current_width = 0;
            continue;
        }

        if ch == '\u{8}' || ch == '\u{7f}' {
            if current.pop().is_some() {
                current_width = UnicodeWidthStrExt::width(current.as_str());
            }
            continue;
        }

        if ch.is_control() && ch != '\t' {
            continue;
        }

        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width > 0 && current_width + ch_width > width {
            output.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }

    output.push(current);
}

pub(super) fn ansi_to_line(input: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if let Some((sequence, final_char)) = read_ansi_sequence(&mut chars) {
                if final_char == 'm' {
                    push_ansi_span(&mut spans, &mut text, style);
                    apply_sgr(&mut style, &sequence);
                }
            }
            continue;
        }

        if ch == '\r' {
            spans.clear();
            text.clear();
            continue;
        }

        if ch == '\u{8}' || ch == '\u{7f}' {
            text.pop();
            continue;
        }

        if ch.is_control() && ch != '\t' {
            continue;
        }

        text.push(ch);
    }

    push_ansi_span(&mut spans, &mut text, style);
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn push_ansi_sequence<I>(first: char, chars: &mut std::iter::Peekable<I>, output: &mut String)
where
    I: Iterator<Item = char>,
{
    output.push(first);
    let Some(next) = chars.next() else {
        return;
    };
    output.push(next);

    match next {
        '[' => {
            for ch in chars.by_ref() {
                output.push(ch);
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        }
        ']' => {
            while let Some(ch) = chars.next() {
                output.push(ch);
                if ch == '\u{7}' {
                    break;
                }
                if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
                    output.push(chars.next().unwrap_or('\\'));
                    break;
                }
            }
        }
        _ => {
            if next == '(' || next == ')' {
                if let Some(ch) = chars.next() {
                    output.push(ch);
                }
            }
        }
    }
}

fn read_ansi_sequence<I>(chars: &mut std::iter::Peekable<I>) -> Option<(String, char)>
where
    I: Iterator<Item = char>,
{
    let introducer = chars.next()?;
    match introducer {
        '[' => read_csi(chars),
        ']' => {
            skip_osc(chars);
            None
        }
        '(' | ')' => {
            let _ = chars.next();
            None
        }
        other => Some((String::new(), other)),
    }
}

fn read_csi<I>(chars: &mut std::iter::Peekable<I>) -> Option<(String, char)>
where
    I: Iterator<Item = char>,
{
    let mut sequence = String::new();
    for ch in chars.by_ref() {
        if ('@'..='~').contains(&ch) {
            return Some((sequence, ch));
        }
        sequence.push(ch);
    }
    None
}

fn skip_osc<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while let Some(ch) = chars.next() {
        if ch == '\u{7}' {
            break;
        }
        if ch == '\u{1b}' && chars.peek() == Some(&'\\') {
            let _ = chars.next();
            break;
        }
    }
}

fn push_ansi_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn apply_sgr(style: &mut Style, sequence: &str) {
    let values = if sequence.trim().is_empty() {
        vec![0]
    } else {
        sequence
            .split(';')
            .map(|part| part.parse::<u16>().unwrap_or(0))
            .collect::<Vec<_>>()
    };

    let mut index = 0;
    while index < values.len() {
        match values[index] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            30..=37 => *style = style.fg(ansi_color(values[index] - 30, false)),
            39 => *style = style.fg(Color::Reset),
            40..=47 => *style = style.bg(ansi_color(values[index] - 40, false)),
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(ansi_color(values[index] - 90, true)),
            100..=107 => *style = style.bg(ansi_color(values[index] - 100, true)),
            38 | 48 => {
                if let Some((color, consumed)) = parse_extended_color(&values[index + 1..]) {
                    if values[index] == 38 {
                        *style = style.fg(color);
                    } else {
                        *style = style.bg(color);
                    }
                    index += consumed;
                }
            }
            _ => {}
        }
        index += 1;
    }
}

fn parse_extended_color(values: &[u16]) -> Option<(Color, usize)> {
    match values {
        [5, color, ..] => Some((Color::Indexed((*color).min(255) as u8), 2)),
        [2, red, green, blue, ..] => Some((
            Color::Rgb(
                (*red).min(255) as u8,
                (*green).min(255) as u8,
                (*blue).min(255) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

fn ansi_color(code: u16, bright: bool) -> Color {
    match (code, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}

struct UnicodeWidthStrExt;

impl UnicodeWidthStrExt {
    fn width(value: &str) -> usize {
        value
            .chars()
            .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ansi_color_spans() {
        let line = ansi_to_line("\u{1b}[31mred\u{1b}[0m plain");
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.spans[0].content.as_ref(), "red");
        assert_eq!(line.spans[0].style.fg, Some(Color::Red));
        assert_eq!(line.spans[1].content.as_ref(), " plain");
    }

    #[test]
    fn strips_non_sgr_terminal_sequences() {
        let line = ansi_to_line("\u{1b}]0;title\u{7}\u{1b}[2K> help");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.as_ref(), "> help");
    }

    #[test]
    fn wraps_long_console_lines_before_rendering() {
        let lines = vec![
            "[00:54:29 INFO]: bStats collects some basic information for plugin authors, like how many people use their plugin and their total player count. It's recommended to keep bStats enabled, but this text must continue."
                .to_string(),
        ];

        let wrapped = wrap_console_lines(&lines, 48);
        let joined = wrapped.join("");

        assert!(wrapped.len() > 1);
        assert!(joined.contains("but this text must continue."));
    }
}
