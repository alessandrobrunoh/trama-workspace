use super::*;

// -- classification -------------------------------------------------------

/// How a tool call is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Edit,
    Command,
    /// A Trama subthread (spawned or messaged): a link row.
    Subthread,
    Minor(Activity),
}

/// What a folded call or thought did, for the summary row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Activity {
    Thought,
    Command,
    Read,
    Search,
    Web,
    Skill,
    Subagent,
    Todo,
    Other,
}

/// The tool's own name, without an `mcp__server__` prefix.
pub(super) fn base_name(name: &str) -> &str {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.rsplit("__").next().unwrap_or(rest),
        None => name,
    }
}

/// MCP server names of Trama's own tools: the daemon's `trama-daemon mcp`
/// helper registers `trama-workspace`; `trama` is accepted as well.
pub(super) const TRAMA_MCP_SERVERS: &[&str] = &["trama-workspace", "trama"];

/// `spawn_subagent` / `send_to_thread` of a Trama MCP server.
pub(super) fn is_trama_subthread_tool(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return false;
    };
    TRAMA_MCP_SERVERS.iter().any(|server| {
        rest.strip_prefix(server)
            .and_then(|tool| tool.strip_prefix("__"))
            .is_some_and(|tool| matches!(tool, "spawn_subagent" | "send_to_thread"))
    })
}

pub fn tool_kind(name: &str) -> ToolKind {
    if is_trama_subthread_tool(name) {
        return ToolKind::Subthread;
    }
    match name {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolKind::Edit,
        "Bash" => ToolKind::Command,
        "Read" | "NotebookRead" => ToolKind::Minor(Activity::Read),
        "Grep" | "Glob" | "LS" | "ToolSearch" => ToolKind::Minor(Activity::Search),
        "WebFetch" | "WebSearch" => ToolKind::Minor(Activity::Web),
        "Skill" => ToolKind::Minor(Activity::Skill),
        "Task" | "Agent" => ToolKind::Minor(Activity::Subagent),
        "TodoWrite" => ToolKind::Minor(Activity::Todo),
        _ => ToolKind::Minor(Activity::Other),
    }
}

pub(super) fn activity(entry: &ChatEntry) -> Option<Activity> {
    match entry {
        ChatEntry::Thinking { .. } => Some(Activity::Thought),
        ChatEntry::ToolCall { name, .. } => match tool_kind(name) {
            ToolKind::Minor(activity) => Some(activity),
            ToolKind::Command => Some(Activity::Command),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn is_edit(entry: &ChatEntry) -> bool {
    matches!(entry, ChatEntry::ToolCall { name, .. } if tool_kind(name) == ToolKind::Edit)
}

/// One transcript block's worth of entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Single(usize),
    /// Thinking, commands and minor tool calls folded into a summary line.
    Group(Range<usize>),
    /// Consecutive file edits, an "Apply patch" line each.
    Edits(Range<usize>),
}

impl Item {
    pub fn start(&self) -> usize {
        match self {
            Item::Single(index) => *index,
            Item::Group(range) | Item::Edits(range) => range.start,
        }
    }
}

/// Fold entries into blocks. A lone step (a thought, a command, a call)
/// keeps its own line.
pub fn layout_items(entries: &[ChatEntry]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let start = index;
        if is_edit(&entries[index]) {
            while index < entries.len() && is_edit(&entries[index]) {
                index += 1;
            }
            items.push(Item::Edits(start..index));
        } else if activity(&entries[index]).is_some() {
            while index < entries.len() && activity(&entries[index]).is_some() {
                index += 1;
            }
            items.push(if index - start == 1 {
                Item::Single(start)
            } else {
                Item::Group(start..index)
            });
        } else {
            items.push(Item::Single(index));
            index += 1;
        }
    }
    items
}

/// "Thought 2 times, loaded a skill, ran 3 commands, read 6 files", in the
/// order each kind first appears.
pub fn group_summary(entries: &[ChatEntry]) -> String {
    let mut order: Vec<Activity> = Vec::new();
    let mut counts: HashMap<Activity, usize> = HashMap::new();
    for activity in entries.iter().filter_map(activity) {
        if !order.contains(&activity) {
            order.push(activity);
        }
        *counts.entry(activity).or_default() += 1;
    }
    let phrase = |activity: Activity, n: usize| -> String {
        let (one, many): (&str, String) = match activity {
            Activity::Thought => ("thought once", format!("thought {n} times")),
            Activity::Command => ("ran a command", format!("ran {n} commands")),
            Activity::Read => ("read a file", format!("read {n} files")),
            Activity::Search => ("ran a search", format!("ran {n} searches")),
            Activity::Web => ("browsed the web", format!("browsed the web {n} times")),
            Activity::Skill => ("loaded a skill", format!("loaded {n} skills")),
            Activity::Subagent => ("ran a subagent", format!("ran {n} subagents")),
            Activity::Todo => ("updated the plan", format!("updated the plan {n} times")),
            Activity::Other => ("called a tool", format!("called {n} tools")),
        };
        if n == 1 { one.to_string() } else { many }
    };
    let text = order
        .iter()
        .map(|activity| phrase(*activity, counts[activity]))
        .collect::<Vec<_>>()
        .join(", ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

// -- tool output ----------------------------------------------------------

/// Text of a tool result: a string, the text blocks of a content array, or
/// the common stdout/stderr fields; other JSON is pretty-printed.
pub fn output_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block {
                Value::String(text) => text.clone(),
                other => other
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| output_text(other)),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            let fields: Vec<&str> = ["stdout", "stderr", "output", "content", "text"]
                .iter()
                .filter_map(|key| map.get(*key).and_then(Value::as_str))
                .filter(|text| !text.is_empty())
                .collect();
            if fields.is_empty() {
                serde_json::to_string_pretty(value).unwrap_or_default()
            } else {
                fields.join("\n")
            }
        }
        other => other.to_string(),
    }
}

/// When the call started and finished, in milliseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTiming {
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
}

impl ToolTiming {
    pub fn duration_ms(&self) -> Option<u64> {
        self.finished_ms
            .map(|end| end.saturating_sub(self.started_ms).max(0) as u64)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// `420ms`, `2.3s`, `1m 04s`.
pub fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.)
    } else {
        let secs = ms / 1_000;
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}
