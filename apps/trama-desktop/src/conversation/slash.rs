//! `/` completion: filtering and ranking the thread's skills and commands.

use trama_protocol::{SkillInfo, SkillKind, SkillSource};

use crate::app::keymap::fuzzy_score;

/// Most rows the popup shows.
pub const MAX_ROWS: usize = 50;

pub fn source_label(source: SkillSource) -> &'static str {
    match source {
        SkillSource::Project => "project",
        SkillSource::User => "user",
        SkillSource::Plugin => "plugin",
        SkillSource::Builtin => "built-in",
    }
}

pub fn kind_label(kind: SkillKind) -> &'static str {
    match kind {
        SkillKind::Skill => "skill",
        SkillKind::Command => "command",
    }
}

/// Closer sources first when everything else ties.
fn source_rank(source: SkillSource) -> u8 {
    match source {
        SkillSource::Project => 0,
        SkillSource::User => 1,
        SkillSource::Plugin => 2,
        SkillSource::Builtin => 3,
    }
}

/// Indices of `items` matching `query`, best first: exact name, name
/// prefix, prefix of the part after a `ns:` namespace, fuzzy name match,
/// then description matches. An empty query lists everything by name.
pub fn filter(items: &[SkillInfo], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut scored: Vec<(usize, (u8, i32))> = items
        .iter()
        .enumerate()
        .filter_map(|(ix, item)| score(item, &query).map(|s| (ix, s)))
        .collect();
    scored.sort_by(|(a_ix, a), (b_ix, b)| {
        let (a_item, b_item) = (&items[*a_ix], &items[*b_ix]);
        a.0.cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(source_rank(a_item.source).cmp(&source_rank(b_item.source)))
            .then(a_item.name.cmp(&b_item.name))
    });
    scored.truncate(MAX_ROWS);
    scored.into_iter().map(|(ix, _)| ix).collect()
}

/// `(tier, score)`: lower tiers rank first, higher scores within a tier.
fn score(item: &SkillInfo, query: &str) -> Option<(u8, i32)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let name = item.name.to_lowercase();
    let short = name
        .rsplit_once(':')
        .map_or(name.as_str(), |(_, short)| short);
    if name == query || short == query {
        return Some((0, 0));
    }
    if name.starts_with(query) {
        return Some((1, -(name.len() as i32)));
    }
    if short.starts_with(query) {
        return Some((2, -(name.len() as i32)));
    }
    if let Some(score) = fuzzy_score(query, &name) {
        return Some((3, score));
    }
    let description = item.description.as_deref().unwrap_or("").to_lowercase();
    description.contains(query).then_some((4, 0))
}

/// Text the composer gets when a row is picked.
pub fn insertion(item: &SkillInfo) -> String {
    format!("/{} ", item.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str, source: SkillSource) -> SkillInfo {
        SkillInfo {
            name: name.into(),
            description: Some(description.into()),
            source,
            kind: SkillKind::Skill,
        }
    }

    fn names(items: &[SkillInfo], query: &str) -> Vec<String> {
        filter(items, query)
            .into_iter()
            .map(|ix| items[ix].name.clone())
            .collect()
    }

    #[test]
    fn empty_query_lists_everything_closest_source_first() {
        let items = vec![
            skill("compact", "Compact the context", SkillSource::Builtin),
            skill("review", "Review a PR", SkillSource::Project),
            skill("deploy", "Ship it", SkillSource::User),
        ];
        assert_eq!(names(&items, ""), vec!["review", "deploy", "compact"]);
    }

    #[test]
    fn prefixes_beat_fuzzy_and_description_matches() {
        let items = vec![
            skill("code-review", "Review code", SkillSource::User),
            skill("review", "Review a PR", SkillSource::Project),
            skill("revert-helper", "Undo things", SkillSource::Plugin),
            skill("my:review-pr", "Review via plugin", SkillSource::Plugin),
            skill(
                "cost",
                "Show what this costs; good for review budgets",
                SkillSource::Builtin,
            ),
            skill("rx", "unrelated", SkillSource::User),
        ];
        let ranked = names(&items, "rev");
        assert_eq!(ranked[0], "review", "shortest name prefix first");
        assert_eq!(ranked[1], "revert-helper");
        assert_eq!(ranked[2], "my:review-pr", "namespace stripped");
        assert!(ranked.contains(&"code-review".to_string()), "fuzzy");
        assert!(!ranked.contains(&"rx".to_string()));
        assert_eq!(names(&items, "budgets"), vec!["cost"], "description");
        assert_eq!(names(&items, "REVIEW")[0], "review", "case-insensitive");
    }

    #[test]
    fn exact_namespaced_name_wins() {
        let items = vec![
            skill("plug:deploy", "", SkillSource::Plugin),
            skill("deployer", "", SkillSource::Project),
        ];
        assert_eq!(names(&items, "deploy"), vec!["plug:deploy", "deployer"]);
    }

    #[test]
    fn picking_inserts_the_command_and_a_space() {
        assert_eq!(
            insertion(&skill("ns:do-it", "", SkillSource::User)),
            "/ns:do-it "
        );
        assert_eq!(source_label(SkillSource::Builtin), "built-in");
        assert_eq!(kind_label(SkillKind::Command), "command");
    }
}
