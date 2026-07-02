use std::fs;
use std::path::{Path, PathBuf};

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

/// Bash/readline-style completion for remiaft's prompt dialogs. `screen`
/// itself never does completion - it just forwards keystrokes to whatever
/// program is running inside it and lets that program's own line editor
/// handle it. remiaft's prompt dialogs (send command, edit startup command,
/// ...) *are* that line editor, so they implement GNU readline's default
/// `rl_complete` behavior as bash configures it (complete.c / bashline.c):
///
///  - the word at point is found honoring bash's completer word-break
///    characters, quote state, and (on Unix) backslash escapes;
///  - the first Tab inserts the longest common prefix of all matches; a
///    unique directory match gets a `/` appended, a unique file match gets
///    a closing quote and a space (space only when point is at end of line);
///  - a second Tab that didn't change the buffer lists the candidates
///    (`rl_last_func == rl_complete && !completion_changed_buffer`);
///  - an empty word matches every entry in the directory including hidden
///    files (readline's `match-hidden-files` defaults to on);
///  - the first word of a command line completes command names from PATH,
///    falling back to filename completion when nothing matches, like bash;
///  - inserted filenames are re-quoted the way `bash_quote_filename` does.
pub(super) enum Completion {
    /// No candidates at all; bash rings the bell.
    NoMatch,
    /// A unique match was inserted together with its `/` or ` ` suffix.
    Unique,
    /// Multiple matches: the longest common prefix was inserted, which may
    /// leave the buffer unchanged; bash rings the bell either way.
    Partial,
}

/// On Unix `\` is bash's escape character; on Windows it has to stay a path
/// separator so `C:\...` and UNC paths survive completion.
const BACKSLASH_ESCAPES: bool = cfg!(not(windows));

fn is_separator(ch: char) -> bool {
    ch == '/' || (cfg!(windows) && ch == '\\')
}

/// bash_completer_word_break_characters (" \t\n\"'@><=;|&(:"). Windows drops
/// ':' from the set so drive-letter prefixes don't get split.
fn is_word_break(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '"' | '\'' | '@' | '>' | '<' | '=' | ';' | '|' | '&' | '('
    ) || (cfg!(not(windows)) && ch == ':')
}

/// rl_filename_quote_characters as bash sets it: characters in a completed
/// filename that need a backslash when inserted outside quotes.
fn needs_escape(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t'
            | '\n'
            | '\\'
            | '"'
            | '\''
            | '@'
            | '<'
            | '>'
            | '='
            | ';'
            | '|'
            | '&'
            | '('
            | ')'
            | '#'
            | '$'
            | '`'
            | '?'
            | '*'
            | '['
            | '!'
            | '{'
            | '~'
    ) || (cfg!(not(windows)) && ch == ':')
}

/// Mirrors readline's `_rl_find_completion_word`: scan the line up to the
/// cursor tracking quote state. If point sits inside an open quoted string
/// the word starts right after the opening quote; otherwise it starts after
/// the closest unescaped word-break character.
fn find_completion_word(line: &str, point: usize) -> (usize, Option<char>) {
    let mut quote: Option<char> = None;
    let mut quote_start = 0;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in line[..point].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                    // a closing quote is a word-break character in bash's
                    // set, so a new word starts right after it
                    start = index + ch.len_utf8();
                } else if q == '"' && ch == '\\' && BACKSLASH_ESCAPES {
                    escaped = true;
                }
            }
            None => {
                if BACKSLASH_ESCAPES && ch == '\\' {
                    escaped = true;
                } else if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    quote_start = index + ch.len_utf8();
                } else if is_word_break(ch) {
                    start = index + ch.len_utf8();
                }
            }
        }
    }
    match quote {
        Some(q) => (quote_start, Some(q)),
        None => (start, None),
    }
}

/// bash_dequote_filename: strip quoting from the word so it can be matched
/// against real directory entries.
fn dequote_word(word: &str, quote_char: Option<char>) -> String {
    if quote_char == Some('\'') {
        return word.to_string();
    }
    let mut result = String::with_capacity(word.len());
    let mut quote = quote_char;
    let mut chars = word.chars();
    while let Some(ch) = chars.next() {
        if BACKSLASH_ESCAPES && ch == '\\' && quote != Some('\'') {
            result.push(chars.next().unwrap_or('\\'));
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && (ch == '"' || ch == '\'') {
            quote = Some(ch);
        } else {
            result.push(ch);
        }
    }
    result
}

/// bash_quote_filename: re-quote a match before inserting it. Inside an open
/// quote the text goes in verbatim. Outside quotes, Unix backslash-escapes
/// the special characters (a leading `~` keeps its tilde meaning); Windows
/// has no escape character, so a match containing a break character gets an
/// opening double quote instead, and the returned quote char lets the suffix
/// logic close it like a user-typed quote.
fn quote_for_insert(text: &str, quote_char: Option<char>) -> (String, Option<char>) {
    if let Some(q) = quote_char {
        return (text.to_string(), Some(q));
    }
    if BACKSLASH_ESCAPES {
        let mut out = String::with_capacity(text.len());
        for (index, ch) in text.char_indices() {
            if needs_escape(ch) && !(index == 0 && ch == '~') {
                out.push('\\');
            }
            out.push(ch);
        }
        (out, None)
    } else if text.chars().any(is_word_break) {
        (format!("\"{text}"), Some('"'))
    } else {
        (text.to_string(), None)
    }
}

#[derive(Clone)]
struct Candidate {
    /// Full replacement for the word: the directory part exactly as the
    /// user typed it plus the matched entry name.
    text: String,
    /// What the candidate list shows: the last pathname component alone,
    /// with `/` appended to directories, like readline's print_filename
    /// with mark-directories on.
    display: String,
    is_dir: bool,
}

fn resolve_lookup_dir(dir_part: &str, directory: &Path) -> PathBuf {
    let path = match dir_part.strip_prefix('~') {
        Some(rest) if rest.chars().next().map(is_separator).unwrap_or(true) => {
            match dirs::home_dir() {
                Some(home) => home.join(rest.trim_start_matches(is_separator)),
                None => PathBuf::from(dir_part),
            }
        }
        _ => PathBuf::from(dir_part),
    };
    if path.is_absolute() {
        path
    } else {
        directory.join(path)
    }
}

/// rl_filename_completion_function: entries of the word's directory whose
/// names start with the typed prefix. An empty prefix matches everything
/// (std's read_dir already omits `.` and `..`, and `match-hidden-files`
/// defaults to on, so dotfiles are included).
fn filename_candidates(prefix: &str, directory: &Path) -> Vec<Candidate> {
    let split = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| is_separator(*ch));
    let (dir_part, name_prefix) = match split {
        Some((index, ch)) => (
            &prefix[..index + ch.len_utf8()],
            &prefix[index + ch.len_utf8()..],
        ),
        None => ("", prefix),
    };
    let Ok(entries) = fs::read_dir(resolve_lookup_dir(dir_part, directory)) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(name_prefix) {
                return None;
            }
            // metadata() follows symlinks, matching readline's stat()-based
            // directory check in append_to_match
            let is_dir = entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false);
            Some(Candidate {
                text: format!("{dir_part}{name}"),
                display: if is_dir { format!("{name}/") } else { name },
                is_dir,
            })
        })
        .collect()
}

/// bash's command_word_completion_function, reduced to the PATH-executable
/// stage that applies here (no aliases/builtins/functions to offer).
fn command_candidates(prefix: &str) -> Vec<Candidate> {
    let Some(paths) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&paths)
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(|entry| entry.ok()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(prefix) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() || !is_executable(&metadata) {
                return None;
            }
            Some(Candidate {
                text: name.clone(),
                display: name,
                is_dir: false,
            })
        })
        .collect()
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

/// Sorted, deduplicated matches for the word, byte order like readline's
/// strcmp sort. In command position bash completes command names first and
/// falls back to filenames only when that produces nothing.
fn collect_candidates(prefix: &str, directory: &Path, command_position: bool) -> Vec<Candidate> {
    let mut candidates = if command_position && !prefix.chars().any(is_separator) {
        let commands = command_candidates(prefix);
        if commands.is_empty() {
            filename_candidates(prefix, directory)
        } else {
            commands
        }
    } else {
        filename_candidates(prefix, directory)
    };
    candidates.sort_by(|a, b| a.text.cmp(&b.text));
    candidates.dedup_by(|a, b| a.text == b.text);
    candidates
}

struct Word {
    start: usize,
    point: usize,
    quote_char: Option<char>,
    candidates: Vec<Candidate>,
}

fn word_at_point(
    input: &str,
    cursor: usize,
    directory: &Path,
    command_position_allowed: bool,
) -> Word {
    let point = normalized_cursor(input, cursor);
    let (start, quote_char) = find_completion_word(input, point);
    let prefix = dequote_word(&input[start..point], quote_char);
    // the opening quote (always one ASCII byte) sits just before the word
    let head_end = start - quote_char.map(|_| 1).unwrap_or(0);
    let command_position = command_position_allowed && input[..head_end].trim().is_empty();
    Word {
        start,
        point,
        quote_char,
        candidates: collect_candidates(&prefix, directory, command_position),
    }
}

fn replace_word(input: &mut String, cursor: &mut usize, start: usize, end: usize, text: &str) {
    input.replace_range(start..end, text);
    *cursor = start + text.len();
}

/// readline's append_to_match: `/` after a unique directory match (skipped
/// when one is already there), otherwise a closing quote plus the completion
/// append character (a space), both only when point is at end of line.
fn append_suffix(input: &mut String, cursor: &mut usize, is_dir: bool, quote_char: Option<char>) {
    if is_dir {
        if input[*cursor..].chars().next().map(is_separator) != Some(true) {
            input.insert(*cursor, '/');
            *cursor += 1;
        }
        return;
    }
    if *cursor == input.len() {
        if let Some(q) = quote_char {
            input.push(q);
            *cursor += 1;
        }
        input.push(' ');
        *cursor += 1;
    }
}

/// rl_complete_internal(TAB): insert the unique match with its suffix, or
/// the longest common prefix of all matches.
pub(super) fn complete_word(
    input: &mut String,
    cursor: &mut usize,
    directory: &Path,
    command_position_allowed: bool,
) -> Completion {
    let word = word_at_point(input, *cursor, directory, command_position_allowed);
    match word.candidates.as_slice() {
        [] => Completion::NoMatch,
        [candidate] => {
            let (quoted, quote_char) = quote_for_insert(&candidate.text, word.quote_char);
            replace_word(input, cursor, word.start, word.point, &quoted);
            append_suffix(input, cursor, candidate.is_dir, quote_char);
            Completion::Unique
        }
        candidates => {
            let texts = candidates
                .iter()
                .map(|candidate| candidate.text.clone())
                .collect::<Vec<_>>();
            let common = longest_common_prefix(&texts);
            let (quoted, _) = quote_for_insert(&common, word.quote_char);
            replace_word(input, cursor, word.start, word.point, &quoted);
            Completion::Partial
        }
    }
}

/// rl_complete_internal('?'): the sorted candidate list a second Tab shows.
pub(super) fn completion_display_candidates(
    input: &str,
    cursor: usize,
    directory: &Path,
    command_position_allowed: bool,
) -> Vec<String> {
    let word = word_at_point(input, cursor, directory, command_position_allowed);
    let mut names = word
        .candidates
        .into_iter()
        .map(|candidate| candidate.display)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("remiaft-completion-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unique_file_match_appends_space() {
        let dir = temp_dir("unique-file");
        fs::write(dir.join("start.sh"), "").unwrap();

        let mut input = "sh sta".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "sh start.sh ");
        assert_eq!(cursor, input.len());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_directory_match_appends_slash_not_space() {
        let dir = temp_dir("unique-dir");
        fs::create_dir_all(dir.join("plugins")).unwrap();

        let mut input = "cd plu".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "cd plugins/");
        assert_eq!(cursor, input.len());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ambiguous_match_inserts_longest_common_prefix() {
        let dir = temp_dir("ambiguous");
        fs::write(dir.join("start.sh"), "").unwrap();
        fs::write(dir.join("start.bak"), "").unwrap();

        let mut input = "sta".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Partial
        ));
        assert_eq!(input, "start.");
        assert_eq!(cursor, input.len());

        assert_eq!(
            completion_display_candidates(&input, cursor, &dir, false),
            vec!["start.bak", "start.sh"]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_word_matches_every_entry() {
        let dir = temp_dir("empty-word");
        fs::write(dir.join("alpha.txt"), "").unwrap();
        fs::create_dir_all(dir.join("world")).unwrap();

        let mut input = "say ".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Partial
        ));
        assert_eq!(input, "say ");

        assert_eq!(
            completion_display_candidates(&input, cursor, &dir, false),
            vec!["alpha.txt", "world/"]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn completes_deeper_path_components() {
        let dir = temp_dir("deep-path");
        fs::create_dir_all(dir.join("plugins")).unwrap();
        fs::write(dir.join("plugins").join("Essentials.jar"), "").unwrap();

        let mut input = "load plugins/Ess".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "load plugins/Essentials.jar ");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quoted_word_completes_and_closes_the_quote() {
        let dir = temp_dir("quoted");
        fs::write(dir.join("My World.txt"), "").unwrap();

        let mut input = "cat \"My Wo".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "cat \"My World.txt\" ");
        assert_eq!(cursor, input.len());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_space_appended_when_cursor_is_mid_line() {
        let dir = temp_dir("mid-line");
        fs::write(dir.join("start.sh"), "").unwrap();

        let mut input = "sta tail".to_string();
        let mut cursor = 3;
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "start.sh tail");
        assert_eq!(cursor, "start.sh".len());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unquoted_match_gets_backslash_escapes() {
        let dir = temp_dir("escape");
        fs::write(dir.join("My World.txt"), "").unwrap();

        let mut input = "cat My".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "cat My\\ World.txt ");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn escaped_prefix_is_dequoted_before_matching() {
        let dir = temp_dir("dequote");
        fs::write(dir.join("My World.txt"), "").unwrap();

        let mut input = "cat My\\ Wo".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "cat My\\ World.txt ");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_match_with_spaces_gets_quoted() {
        let dir = temp_dir("win-quote");
        fs::write(dir.join("My World.txt"), "").unwrap();

        let mut input = "cat My".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "cat \"My World.txt\" ");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_accepts_backslash_as_separator() {
        let dir = temp_dir("win-sep");
        fs::create_dir_all(dir.join("plugins")).unwrap();
        fs::write(dir.join("plugins").join("Essentials.jar"), "").unwrap();

        let mut input = "load plugins\\Ess".to_string();
        let mut cursor = input.len();
        assert!(matches!(
            complete_word(&mut input, &mut cursor, &dir, false),
            Completion::Unique
        ));
        assert_eq!(input, "load plugins\\Essentials.jar ");

        fs::remove_dir_all(&dir).unwrap();
    }
}
