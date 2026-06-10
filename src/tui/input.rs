use std::fs;
use std::path::Path;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn fallback<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.trim().is_empty() {
        default
    } else {
        value.trim()
    }
}

pub(super) fn input_view(value: &str, cursor: usize, width: usize) -> (String, u16) {
    let cursor = normalized_cursor(value, cursor);
    let width = width.max(1);
    let mut start = cursor;
    let mut used_width = 0;
    let before_cursor = &value[..cursor];

    for (index, ch) in before_cursor.char_indices().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used_width + ch_width > width.saturating_sub(1) {
            break;
        }
        start = index;
        used_width += ch_width;
    }

    let cursor_col = UnicodeWidthStr::width(&value[start..cursor]).min(width) as u16;
    let mut end = cursor;
    let mut total_width = cursor_col as usize;
    for (offset, ch) in value[cursor..].char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if total_width + ch_width > width {
            break;
        }
        end = cursor + offset + ch.len_utf8();
        total_width += ch_width;
    }

    (value[start..end].to_string(), cursor_col)
}

pub(super) fn normalized_cursor(value: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(value.len());
    while cursor > 0 && !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

pub(super) fn insert_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    *cursor = normalized_cursor(value, *cursor);
    value.insert(*cursor, ch);
    *cursor += ch.len_utf8();
}

pub(super) fn delete_at_cursor(value: &mut String, cursor: usize) {
    let cursor = normalized_cursor(value, cursor);
    if cursor >= value.len() {
        return;
    }
    let next = value[cursor..]
        .chars()
        .next()
        .map(|ch| cursor + ch.len_utf8())
        .unwrap_or(value.len());
    value.drain(cursor..next);
}

pub(super) fn backspace_at_cursor(value: &mut String, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor == 0 {
        return;
    }
    let previous = value[..*cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
    value.drain(previous..*cursor);
    *cursor = previous;
}

pub(super) fn move_cursor_left(value: &str, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor == 0 {
        return;
    }
    *cursor = value[..*cursor]
        .char_indices()
        .last()
        .map(|(index, _)| index)
        .unwrap_or(0);
}

pub(super) fn move_cursor_right(value: &str, cursor: &mut usize) {
    *cursor = normalized_cursor(value, *cursor);
    if *cursor >= value.len() {
        return;
    }
    if let Some(ch) = value[*cursor..].chars().next() {
        *cursor += ch.len_utf8();
    }
}

pub(super) fn complete_input_token(
    input: &mut String,
    cursor: &mut usize,
    directory: &Path,
) -> bool {
    let cursor_pos = (*cursor).min(input.len());
    if !input.is_char_boundary(cursor_pos) {
        return false;
    }
    let start = input[..cursor_pos]
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    let prefix = &input[start..cursor_pos];
    if prefix.is_empty() {
        return false;
    }

    let is_first_token = input[..start].trim().is_empty();
    let Some(completion) = complete_token(prefix, directory, is_first_token) else {
        return false;
    };

    input.replace_range(start..cursor_pos, &completion);
    *cursor = start + completion.len();
    true
}

fn complete_token(prefix: &str, directory: &Path, is_first_token: bool) -> Option<String> {
    let mut candidates = path_completion_candidates(prefix, directory);
    if is_first_token && !prefix.contains('/') {
        candidates.extend(path_command_candidates(prefix));
    }
    candidates.sort();
    candidates.dedup();

    match candidates.as_slice() {
        [] => None,
        [candidate] => Some(candidate.clone()),
        _ => {
            let common = longest_common_prefix(&candidates);
            (common.len() > prefix.len()).then_some(common)
        }
    }
}

fn path_completion_candidates(prefix: &str, directory: &Path) -> Vec<String> {
    let (dir_prefix, name_prefix) = prefix
        .rfind('/')
        .map(|index| (&prefix[..=index], &prefix[index + 1..]))
        .unwrap_or(("", prefix));
    let lookup_dir = if dir_prefix.is_empty() {
        directory.to_path_buf()
    } else {
        let path = Path::new(dir_prefix);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            directory.join(path)
        }
    };

    let Ok(entries) = fs::read_dir(lookup_dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(name_prefix) {
                return None;
            }
            let suffix = entry
                .file_type()
                .ok()
                .filter(|file_type| file_type.is_dir())
                .map(|_| "/")
                .unwrap_or(" ");
            Some(format!("{dir_prefix}{name}{suffix}"))
        })
        .collect()
}

fn path_command_candidates(prefix: &str) -> Vec<String> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .filter_map(|path| fs::read_dir(path).ok())
        .flat_map(|entries| entries.filter_map(|entry| entry.ok()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) || !entry.file_type().ok()?.is_file() {
                return None;
            }
            Some(format!("{name} "))
        })
        .collect()
}

fn longest_common_prefix(values: &[String]) -> String {
    let mut prefix = values.first().cloned().unwrap_or_default();
    for value in &values[1..] {
        while !prefix.is_empty() && !value.starts_with(&prefix) {
            prefix.pop();
        }
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_file_token_from_server_directory() {
        let dir =
            std::env::temp_dir().join(format!("remiaft-completion-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("start.sh"), "").unwrap();

        let mut input = "sh sta".to_string();
        let mut cursor = input.len();

        assert!(complete_input_token(&mut input, &mut cursor, &dir));
        assert_eq!(input, "sh start.sh ");
        assert_eq!(cursor, input.len());

        fs::remove_dir_all(&dir).unwrap();
    }
}
