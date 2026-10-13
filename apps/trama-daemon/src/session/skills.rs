//! `/` completion sources: skill and command files on disk.
//!
//! Discovery is best effort and bounded. A file or directory is read only
//! when its canonical path stays inside the root it was found under (or an
//! explicitly allowed target root), so a symlink cannot pull in files from
//! elsewhere on the machine.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use trama_protocol::{SkillInfo, SkillKind, SkillSource};

/// Largest SKILL.md / command file read (only the frontmatter matters).
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Entries read per skills or commands directory.
const MAX_DIR_ENTRIES: usize = 500;
/// Directories visited while looking for plugin skills.
const MAX_PLUGIN_DIRS: usize = 4_000;
const MAX_PLUGIN_DEPTH: usize = 7;
const MAX_DESCRIPTION_CHARS: usize = 400;

/// Built-in commands known to work in headless (`-p` stream-json) sessions.
/// The agent's own `system/init` list adds the rest once a session started.
pub(super) const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    (
        "compact",
        "Summarize the conversation so far to free up context",
    ),
    ("context", "Show how the context window is used"),
    ("cost", "Show token usage and cost of this session"),
];

/// Where to look. `home` is the user's home directory (`None` skips user
/// and plugin sources).
pub(super) struct SkillRoots {
    pub checkouts: Vec<PathBuf>,
    pub home: Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Every skill and command found under `roots`, deduplicated by name (the
/// first source wins: project, then user, then plugin).
pub(super) fn discover(roots: &SkillRoots) -> Vec<SkillInfo> {
    let mut found: BTreeMap<String, SkillInfo> = BTreeMap::new();
    let mut add = |items: Vec<SkillInfo>| {
        for item in items {
            found.entry(item.name.clone()).or_insert(item);
        }
    };
    for checkout in &roots.checkouts {
        let allowed = [checkout.clone()];
        for dir in [".claude/skills", ".agents/skills"] {
            add(scan_skills(
                &checkout.join(dir),
                &allowed,
                SkillSource::Project,
                None,
            ));
        }
        add(scan_commands(
            &checkout.join(".claude/commands"),
            &allowed,
            SkillSource::Project,
            None,
        ));
    }
    if let Some(home) = &roots.home {
        let skills = home.join(".claude/skills");
        // Skills installed for several agents are commonly symlinked from
        // ~/.agents/skills; that target root is allowed, nothing else is.
        let allowed = [skills.clone(), home.join(".agents/skills")];
        add(scan_skills(&skills, &allowed, SkillSource::User, None));
        let commands = home.join(".claude/commands");
        add(scan_commands(
            &commands,
            std::slice::from_ref(&commands),
            SkillSource::User,
            None,
        ));
        add(scan_plugins(&home.join(".claude/plugins")));
    }
    found.into_values().collect()
}

/// `true` when `path` resolves inside one of `allowed`.
fn confined(path: &Path, allowed: &[PathBuf]) -> bool {
    let Ok(real) = dunce::canonicalize(path) else {
        return false;
    };
    allowed.iter().any(|root| {
        dunce::canonicalize(root)
            .map(|root| real.starts_with(root))
            .unwrap_or(false)
    })
}

fn read_bounded(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .take(MAX_DIR_ENTRIES)
        .collect();
    paths.sort();
    paths
}

/// `<dir>/<name>/SKILL.md` entries.
fn scan_skills(
    dir: &Path,
    allowed: &[PathBuf],
    source: SkillSource,
    namespace: Option<&str>,
) -> Vec<SkillInfo> {
    if !dir.is_dir() || !confined(dir, allowed) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for skill_dir in entries(dir) {
        let file = skill_dir.join("SKILL.md");
        if !skill_dir.is_dir() || !confined(&skill_dir, allowed) || !confined(&file, allowed) {
            continue;
        }
        let Some(text) = read_bounded(&file) else {
            continue;
        };
        let frontmatter = parse_frontmatter(&text);
        let fallback = skill_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let Some(name) = frontmatter.name.or(fallback).filter(|n| valid_name(n)) else {
            continue;
        };
        out.push(SkillInfo {
            name: qualified(namespace, &name),
            description: frontmatter.description,
            source,
            kind: SkillKind::Skill,
        });
    }
    out
}

/// `<dir>/<name>.md` and one nested level `<dir>/<ns>/<name>.md` (`ns:name`).
fn scan_commands(
    dir: &Path,
    allowed: &[PathBuf],
    source: SkillSource,
    namespace: Option<&str>,
) -> Vec<SkillInfo> {
    if !dir.is_dir() || !confined(dir, allowed) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut visit = |path: &Path, prefix: Option<String>| {
        if path.extension().and_then(|ext| ext.to_str()) != Some("md")
            || !path.is_file()
            || !confined(path, allowed)
        {
            return;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            return;
        };
        let name = match &prefix {
            Some(prefix) => format!("{prefix}:{stem}"),
            None => stem,
        };
        if !valid_name(&name) {
            return;
        }
        let text = read_bounded(path).unwrap_or_default();
        let frontmatter = parse_frontmatter(&text);
        let description = frontmatter
            .description
            .or_else(|| first_body_line(&text).map(|line| clean_description(&line)));
        out.push(SkillInfo {
            name: qualified(namespace, &name),
            description,
            source,
            kind: SkillKind::Command,
        });
    };
    for path in entries(dir) {
        if path.is_dir() {
            if !confined(&path, allowed) {
                continue;
            }
            let prefix = path.file_name().map(|n| n.to_string_lossy().into_owned());
            for nested in entries(&path) {
                visit(&nested, prefix.clone());
            }
        } else {
            visit(&path, None);
        }
    }
    out
}

/// Plugin skills and commands anywhere (bounded) under `root`: every
/// `skills/` or `commands/` directory, named `<plugin>:<name>`. Symlinks
/// are never followed during the walk.
fn scan_plugins(root: &Path) -> Vec<SkillInfo> {
    if !root.is_dir() {
        return Vec::new();
    }
    let allowed = [root.to_path_buf()];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > MAX_PLUGIN_DIRS {
            break;
        }
        for path in entries(&dir) {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name == "skills" || name == "commands" {
                let plugin = plugin_name(&dir);
                if name == "skills" {
                    out.extend(scan_skills(
                        &path,
                        &allowed,
                        SkillSource::Plugin,
                        Some(&plugin),
                    ));
                } else {
                    out.extend(scan_commands(
                        &path,
                        &allowed,
                        SkillSource::Plugin,
                        Some(&plugin),
                    ));
                }
                continue;
            }
            if depth + 1 < MAX_PLUGIN_DEPTH && name != "node_modules" && name != ".git" {
                stack.push((path, depth + 1));
            }
        }
    }
    out
}

/// `.claude-plugin/plugin.json` `name`, else the directory name.
fn plugin_name(dir: &Path) -> String {
    let manifest = dir.join(".claude-plugin/plugin.json");
    if let Some(name) = read_bounded(&manifest)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|json| json.get("name")?.as_str().map(str::to_owned))
        .filter(|name| valid_name(name))
    {
        return name;
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".into())
}

fn qualified(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(namespace) if !name.starts_with(&format!("{namespace}:")) => {
            format!("{namespace}:{name}")
        }
        _ => name.to_owned(),
    }
}

/// Names usable after `/`: no whitespace or path separators.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

fn first_body_line(text: &str) -> Option<String> {
    let body = match text.strip_prefix("---") {
        Some(rest) => rest.split_once("\n---").map(|(_, body)| body).unwrap_or(""),
        None => text,
    };
    body.lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty() && *line != "-")
        .map(str::to_owned)
}

fn clean_description(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX_DESCRIPTION_CHARS {
        let mut cut: String = collapsed.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        collapsed
    }
}

/// `name` and `description` from a leading YAML frontmatter block. Handles
/// the subset skill files use: plain, single- and double-quoted scalars,
/// folded (`>`) and literal (`|`) blocks, and indented continuation lines.
pub(super) fn parse_frontmatter(text: &str) -> Frontmatter {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Frontmatter::default();
    }
    let block: Vec<&str> = lines
        .take_while(|line| !matches!(line.trim_end(), "---" | "..."))
        .collect();
    let mut out = Frontmatter::default();
    let mut index = 0;
    while index < block.len() {
        let line = block[index];
        index += 1;
        if line.starts_with([' ', '\t']) || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key != "name" && key != "description" {
            continue;
        }
        let value = value.trim();
        // Indented lines that follow belong to this key.
        let mut continuation = Vec::new();
        while index < block.len()
            && (block[index].starts_with([' ', '\t']) || block[index].trim().is_empty())
        {
            continuation.push(block[index].trim());
            index += 1;
        }
        let parsed = if value.starts_with('|') {
            continuation.join("\n").trim().to_owned()
        } else if value.starts_with('>') || value.is_empty() {
            continuation
                .iter()
                .filter(|line| !line.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            let mut scalar = unquote(value);
            for line in continuation.iter().filter(|line| !line.is_empty()) {
                scalar.push(' ');
                scalar.push_str(line);
            }
            scalar
        };
        if parsed.is_empty() {
            continue;
        }
        match key {
            "name" => out.name = Some(parsed.trim().to_owned()),
            _ => out.description = Some(clean_description(&parsed)),
        }
    }
    out
}

fn unquote(value: &str) -> String {
    if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|rest| rest.rfind('"').map(|end| &rest[..end]))
    {
        return inner
            .replace("\\\"", "\"")
            .replace("\\n", " ")
            .replace("\\\\", "\\");
    }
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|rest| rest.rfind('\'').map(|end| &rest[..end]))
    {
        return inner.replace("''", "'");
    }
    // Plain scalar: a ` #` starts a comment.
    match value.find(" #") {
        Some(at) => value[..at].trim_end().to_owned(),
        None => value.to_owned(),
    }
}
