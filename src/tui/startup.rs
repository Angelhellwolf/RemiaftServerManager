use std::path::{Path, PathBuf};

use crate::config::ServerConfig;

pub(super) struct NormalizedStartup {
    pub(super) java_path: Option<String>,
    pub(super) jar_path: Option<PathBuf>,
    pub(super) min_memory_mb: Option<u32>,
    pub(super) max_memory_mb: Option<u32>,
    pub(super) java_args: Vec<String>,
    pub(super) server_args: Vec<String>,
    pub(super) changed: bool,
}

pub(super) fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escape = false;

    for ch in input.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }

        match ch {
            '\\' => escape = true,
            '"' | '\'' if quote == Some(ch) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(ch),
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if escape {
        current.push('\\');
    }
    if !current.is_empty() {
        args.push(current);
    }

    args
}

pub(super) fn parse_startup_command(command: &str, _server_dir: &Path) -> NormalizedStartup {
    normalize_startup_parts(split_args(command))
}

pub(super) fn apply_startup_command(
    server: &mut ServerConfig,
    parsed: NormalizedStartup,
    command: &str,
) {
    if let Some(java_path) = parsed.java_path {
        server.java_path = Some(java_path);
    }
    if let Some(jar_path) = parsed.jar_path {
        server.jar_path = jar_path;
    }
    if let Some(min_memory_mb) = parsed.min_memory_mb {
        server.min_memory_mb = min_memory_mb;
    }
    if let Some(max_memory_mb) = parsed.max_memory_mb {
        server.max_memory_mb = max_memory_mb;
    }
    server.java_args = parsed.java_args;
    server.server_args = parsed.server_args;
    server.startup_command = Some(command.to_string());
}

pub(super) fn normalize_startup_parts(parts: Vec<String>) -> NormalizedStartup {
    let Some(jar_index) = parts.iter().position(|part| part == "-jar") else {
        return NormalizedStartup {
            java_path: None,
            jar_path: None,
            min_memory_mb: None,
            max_memory_mb: None,
            java_args: Vec::new(),
            server_args: Vec::new(),
            changed: false,
        };
    };

    let java_path = parts
        .first()
        .filter(|part| looks_like_java_bin(part))
        .cloned();
    let java_arg_start = usize::from(java_path.is_some());
    let jar_path = parts.get(jar_index + 1).map(PathBuf::from);
    let mut min_memory_mb = None;
    let mut max_memory_mb = None;
    let mut java_args = Vec::new();
    for arg in &parts[java_arg_start..jar_index] {
        if let Some(value) = arg.strip_prefix("-Xms").and_then(parse_memory_mb) {
            min_memory_mb = Some(value);
        } else if let Some(value) = arg.strip_prefix("-Xmx").and_then(parse_memory_mb) {
            max_memory_mb = Some(value);
        } else {
            java_args.push(arg.clone());
        }
    }
    let server_args = parts.get(jar_index + 2..).unwrap_or(&[]).to_vec();

    NormalizedStartup {
        java_path,
        jar_path,
        min_memory_mb,
        max_memory_mb,
        java_args,
        server_args,
        changed: true,
    }
}

fn parse_memory_mb(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (number, multiplier) = match value
        .chars()
        .last()
        .map(|ch| ch.to_ascii_uppercase())
        .unwrap_or('M')
    {
        'G' => (&value[..value.len() - 1], 1024),
        'M' => (&value[..value.len() - 1], 1),
        _ => (value, 1),
    };
    number.parse::<u32>().ok().map(|mb| mb * multiplier)
}

fn looks_like_java_bin(value: &str) -> bool {
    let name = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value);
    name == "java" || name.starts_with("java")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_startup_command() {
        let parsed = parse_startup_command(
            "java25 -Xms1G -Xmx4096M -Dfoo=bar -jar velocity.jar nogui",
            Path::new("."),
        );

        assert_eq!(parsed.java_path.as_deref(), Some("java25"));
        assert_eq!(parsed.min_memory_mb, Some(1024));
        assert_eq!(parsed.max_memory_mb, Some(4096));
        assert_eq!(parsed.jar_path.as_deref(), Some(Path::new("velocity.jar")));
        assert_eq!(parsed.java_args, vec!["-Dfoo=bar"]);
        assert_eq!(parsed.server_args, vec!["nogui"]);
    }

    #[test]
    fn keeps_non_java_startup_command_out_of_java_args() {
        let parsed = parse_startup_command("sh start.sh", Path::new("."));

        assert_eq!(parsed.java_path, None);
        assert_eq!(parsed.jar_path, None);
        assert_eq!(parsed.java_args, Vec::<String>::new());
        assert_eq!(parsed.server_args, Vec::<String>::new());
        assert!(!parsed.changed);
    }
}
