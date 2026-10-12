//! Who is in a shared thread, and the pieces of the transcript that show it:
//! title bar avatars, remote authors, live drafts, other people's running
//! turns, the revoked banner and the execution footer.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex, spinner::Spinner, tooltip::Tooltip, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::message::Role;
use trama_protocol::{CollabMember, CollabMemberVia, CollabPresenceState};

use crate::collab::{RemotePhase, RemoteTurn, Speaker, avatar_hue, initials};
use crate::theme::{self, Palette};

/// Avatars shown in the title bar before the rest collapse into `+N`.
const MAX_TITLE_AVATARS: usize = 5;

/// One person in a shared thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub username: String,
    /// `None` for someone seen live but not (yet) in the member list.
    pub via: Option<CollabMemberVia>,
    pub state: CollabPresenceState,
    pub owner: bool,
    pub me: bool,
    pub typing: bool,
}

impl Participant {
    pub fn here(&self) -> bool {
        self.me || is_here(self.state)
    }

    pub fn tooltip(&self) -> String {
        let mut parts = vec![self.username.clone()];
        if self.me {
            parts.push("you".into());
        }
        if self.owner {
            parts.push("owner".into());
        }
        parts.push(if self.me {
            "here".into()
        } else if self.typing {
            "typing".into()
        } else {
            state_label(self.state).to_string()
        });
        parts.join(" · ")
    }
}

pub fn is_here(state: CollabPresenceState) -> bool {
    matches!(
        state,
        CollabPresenceState::Online | CollabPresenceState::Active
    )
}

pub fn state_label(state: CollabPresenceState) -> &'static str {
    match state {
        CollabPresenceState::Active => "active",
        CollabPresenceState::Online => "online",
        CollabPresenceState::Idle => "idle",
        CollabPresenceState::Offline => "offline",
    }
}

fn state_rank(participant: &Participant) -> u8 {
    if participant.me {
        return 0;
    }
    match participant.state {
        CollabPresenceState::Active => 0,
        CollabPresenceState::Online => 1,
        CollabPresenceState::Idle => 2,
        CollabPresenceState::Offline => 3,
    }
}

/// Merge the member list with live presence. Live keys are lowercase
/// usernames (the service compares names case-insensitively). The owner
/// comes first, then whoever is here, then by name; people seen live but
/// missing from a stale member list are kept.
pub fn roster(
    members: &[CollabMember],
    owner: &str,
    live: &HashMap<String, CollabPresenceState>,
    me: Option<&str>,
    typing: &HashSet<String>,
) -> Vec<Participant> {
    let is_me = |name: &str| me.is_some_and(|me| me.eq_ignore_ascii_case(name));
    let mut people: Vec<Participant> = members
        .iter()
        .map(|member| {
            let key = member.username.to_lowercase();
            Participant {
                username: member.username.clone(),
                via: Some(member.via),
                state: live
                    .get(&key)
                    .copied()
                    .unwrap_or(CollabPresenceState::Offline),
                owner: member.username.eq_ignore_ascii_case(owner),
                me: is_me(&member.username),
                typing: typing.contains(&key),
            }
        })
        .collect();
    let mut extra: Vec<_> = live
        .iter()
        .filter(|(name, state)| {
            is_here(**state) && !people.iter().any(|p| p.username.eq_ignore_ascii_case(name))
        })
        .collect();
    extra.sort_by(|a, b| a.0.cmp(b.0));
    for (name, state) in extra {
        people.push(Participant {
            username: name.clone(),
            via: None,
            state: *state,
            owner: name.eq_ignore_ascii_case(owner),
            me: is_me(name),
            typing: typing.contains(name),
        });
    }
    people.sort_by(|a, b| {
        (!a.owner, state_rank(a), a.username.to_lowercase()).cmp(&(
            !b.owner,
            state_rank(b),
            b.username.to_lowercase(),
        ))
    });
    people
}

// -- elements ---------------------------------------------------------------------

/// A round avatar with the person's initials, tinted by a stable hue.
/// `dim` greys it out for people who are away.
pub fn avatar(p: &Palette, name: &str, size: f32, dim: bool) -> Div {
    let hue = avatar_hue(name);
    let dark = p.bg.l < 0.5;
    let (bg, fg) = if dark {
        (hsla(hue, 0.45, 0.32, 1.), hsla(hue, 0.70, 0.86, 1.))
    } else {
        (hsla(hue, 0.65, 0.88, 1.), hsla(hue, 0.60, 0.32, 1.))
    };
    div()
        .size(px(size))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(bg)
        .text_color(fg)
        .text_size(px((size * 0.42).max(8.)))
        .font_weight(FontWeight::SEMIBOLD)
        .when(dim, |this| this.opacity(0.45))
        .child(initials(name))
}

pub fn owner_badge(p: &Palette) -> impl IntoElement {
    h_flex()
        .h(px(18.))
        .px(theme::sp(1.5))
        .gap(theme::sp(1.))
        .items_center()
        .rounded(theme::radius_sm())
        .border_1()
        .border_color(p.border)
        .text_size(theme::text_xs())
        .text_color(p.text_muted)
        .child(Icon::new(IconName::Crown).with_size(px(10.)))
        .child("Owner")
}

/// Overlapping title bar avatars: present people bright, away people dim, a
/// dot on whoever is typing, a crown on the owner.
pub fn title_avatars(p: &Palette, people: &[Participant]) -> AnyElement {
    let shown = people.len().min(MAX_TITLE_AVATARS);
    let rest = people.len() - shown;
    let ring = p.panel;
    h_flex()
        .id("collaborators")
        .flex_shrink_0()
        .pl(px(6.))
        .items_center()
        .children(people.iter().take(shown).map(|person| {
            let tooltip: SharedString = person.tooltip().into();
            div()
                .id(SharedString::from(format!("avatar-{}", person.username)))
                .relative()
                .ml(px(-6.))
                .p(px(1.5))
                .rounded_full()
                .bg(ring)
                .child(avatar(p, &person.username, 22., !person.here()))
                .when(person.owner, |this| {
                    this.child(
                        div().absolute().top(px(-5.)).left(px(8.)).child(
                            Icon::new(IconName::Crown)
                                .with_size(px(9.))
                                .text_color(p.warning),
                        ),
                    )
                })
                .when(person.typing || (person.here() && !person.me), |this| {
                    this.child(
                        div()
                            .absolute()
                            .bottom(px(0.))
                            .right(px(0.))
                            .size(px(7.))
                            .rounded_full()
                            .border_1()
                            .border_color(ring)
                            .bg(if person.typing { p.accent } else { p.success }),
                    )
                })
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        }))
        .when(rest > 0, |this| {
            this.child(
                div().ml(px(-6.)).p(px(1.5)).rounded_full().bg(ring).child(
                    div()
                        .size(px(22.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(p.selected)
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(format!("+{rest}")),
                ),
            )
        })
        .into_any_element()
}

/// The author line of another participant's message.
pub fn person_row(p: &Palette, name: &str) -> impl IntoElement {
    h_flex()
        .gap(theme::sp(2.))
        .items_center()
        .child(avatar(p, name, 20., false))
        .child(
            div()
                .text_size(theme::text_sm())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(name.to_string()),
        )
}

/// The author line of another participant's Claude: their avatar with a
/// small bot badge.
pub fn remote_agent_row(p: &Palette, name: &str) -> impl IntoElement {
    h_flex()
        .gap(theme::sp(2.))
        .items_center()
        .child(
            div().relative().child(avatar(p, name, 20., false)).child(
                div()
                    .absolute()
                    .bottom(px(-3.))
                    .right(px(-3.))
                    .size(px(12.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(p.accent_soft)
                    .border_1()
                    .border_color(p.bg)
                    .child(
                        Icon::new(IconName::Bot)
                            .with_size(px(8.))
                            .text_color(p.accent_text),
                    ),
            ),
        )
        .child(
            div()
                .text_size(theme::text_sm())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text)
                .child(Speaker::Remote(name.to_string()).label(Role::Assistant)),
        )
        .child(
            div()
                .text_size(theme::text_xs())
                .text_color(p.text_faint)
                .child("ran on their machine"),
        )
}

/// A mirrored notice from another participant (a comment, a verdict, a
/// change summary).
pub fn remote_notice(p: &Palette, name: &str, text: &str) -> AnyElement {
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.5))
        .items_start()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.border)
        .bg(p.elevated)
        .text_size(theme::text_sm())
        .line_height(relative(1.5))
        .text_color(p.text_muted)
        .child(avatar(p, name, 18., false))
        .child(div().flex_1().min_w_0().child(text.to_string()))
        .into_any_element()
}

/// Another participant's turn running on their machine, streamed live.
pub fn remote_turn(p: &Palette, turn: &RemoteTurn) -> AnyElement {
    let status = match turn.phase {
        RemotePhase::Waiting => format!("Waiting for {}'s machine to pick this up…", turn.author),
        RemotePhase::Running => format!("Working on {}'s machine…", turn.author),
    };
    let text = turn.text.trim();
    v_flex()
        .w_full()
        .gap(theme::sp(2.))
        .child(remote_agent_row(p, &turn.author))
        .child(
            v_flex()
                .pl(theme::sp(7.))
                .gap(theme::sp(1.5))
                .when(!text.is_empty(), |this| {
                    this.child(
                        div()
                            .font_family(theme::PROSE_FONT)
                            .text_size(theme::text_prose())
                            .line_height(relative(1.55))
                            .text_color(p.text)
                            .child(text.to_string()),
                    )
                })
                .children(turn.tools.iter().map(|tool| {
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .text_size(theme::text_sm())
                        .text_color(p.text_muted)
                        .child(
                            Icon::new(if !tool.finished {
                                IconName::LoaderCircle
                            } else if tool.is_error {
                                IconName::CircleX
                            } else {
                                IconName::Check
                            })
                            .with_size(px(12.))
                            .text_color(if tool.is_error {
                                p.danger
                            } else {
                                p.text_faint
                            }),
                        )
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(p.text)
                                .child(tool.name.clone()),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(theme::MONO_FONT)
                                .text_size(theme::text_xs())
                                .child(tool.summary.clone()),
                        )
                }))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .text_size(theme::text_sm())
                        .text_color(p.text_muted)
                        .child(Spinner::new().small().color(p.accent))
                        .child(status),
                ),
        )
        .into_any_element()
}

/// Others' live drafts, faded, above the composer.
pub fn drafts_block(p: &Palette, drafts: &[(String, String)]) -> Option<AnyElement> {
    if drafts.is_empty() {
        return None;
    }
    Some(
        v_flex()
            .w_full()
            .pb(theme::sp(2.))
            .gap(theme::sp(1.5))
            .children(drafts.iter().map(|(name, text)| {
                let preview: String = {
                    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if flat.chars().count() > 280 {
                        let cut: String = flat.chars().take(280).collect();
                        format!("{cut}…")
                    } else {
                        flat
                    }
                };
                h_flex()
                    .w_full()
                    .px(theme::sp(3.))
                    .py(theme::sp(2.))
                    .gap(theme::sp(2.5))
                    .items_start()
                    .rounded(theme::radius_lg())
                    .border_1()
                    .border_dashed()
                    .border_color(p.border_strong)
                    .bg(p.elevated.opacity(0.6))
                    .opacity(0.8)
                    .child(avatar(p, name, 20., false))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                h_flex()
                                    .gap(theme::sp(1.5))
                                    .items_center()
                                    .text_size(theme::text_xs())
                                    .text_color(p.text_faint)
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text_muted)
                                            .child(name.clone()),
                                    )
                                    .child("is typing"),
                            )
                            .child(
                                div()
                                    .text_size(theme::text_ui())
                                    .line_height(relative(1.45))
                                    .text_color(p.text_muted)
                                    .italic()
                                    .child(preview),
                            ),
                    )
            }))
            .into_any_element(),
    )
}

pub fn revoked_banner(p: &Palette) -> AnyElement {
    h_flex()
        .w_full()
        .px(theme::sp(3.))
        .py(theme::sp(2.))
        .gap(theme::sp(2.))
        .items_center()
        .rounded(theme::radius())
        .border_1()
        .border_color(p.danger.opacity(0.30))
        .bg(p.danger.opacity(0.08))
        .text_size(theme::text_sm())
        .text_color(p.text)
        .child(
            Icon::new(IconName::Lock)
                .with_size(px(14.))
                .text_color(p.danger),
        )
        .child(
            div()
                .flex_1()
                .child("You no longer have access to this shared thread; local history kept."),
        )
        .into_any_element()
}

/// The subtle line under the composer of a shared thread (ADR 0008).
pub fn shared_footer(p: &Palette, runner_online: bool) -> AnyElement {
    h_flex()
        .w_full()
        .pt(theme::sp(1.5))
        .px(theme::sp(1.))
        .gap(theme::sp(1.5))
        .items_center()
        .text_size(theme::text_xs())
        .text_color(p.text_faint)
        .child(Icon::new(IconName::Users).with_size(px(11.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child("Shared thread · each person's turns run on their own machine with their own Claude Code"),
        )
        .when(!runner_online, |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_color(p.warning)
                    .child("your runner is offline"),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{CollabMember, CollabMemberVia, CollabPresenceState, HashMap, HashSet, roster};

    fn member(name: &str, via: CollabMemberVia) -> CollabMember {
        CollabMember {
            username: name.into(),
            via,
        }
    }

    #[test]
    fn roster_merges_members_with_live_presence() {
        let members = vec![
            member("zoe", CollabMemberVia::Invite),
            member("Alice", CollabMemberVia::Owner),
            member("bob", CollabMemberVia::Link),
            member("me", CollabMemberVia::Invite),
        ];
        let live = HashMap::from([
            ("bob".to_string(), CollabPresenceState::Idle),
            ("zoe".to_string(), CollabPresenceState::Active),
            ("carl".to_string(), CollabPresenceState::Online),
            ("gone".to_string(), CollabPresenceState::Offline),
        ]);
        let typing = HashSet::from(["zoe".to_string()]);
        let people = roster(&members, "alice", &live, Some("ME"), &typing);
        let names: Vec<&str> = people.iter().map(|p| p.username.as_str()).collect();
        assert_eq!(names, vec!["Alice", "me", "zoe", "carl", "bob"]);

        let alice = &people[0];
        assert!(alice.owner);
        assert_eq!(alice.state, CollabPresenceState::Offline);
        assert!(!alice.here());
        assert!(people[1].me && people[1].here());
        assert!(people[2].typing);
        assert_eq!(people[3].via, None, "seen live, not in the member list");
        assert_eq!(people[4].state, CollabPresenceState::Idle);
        assert!(!people.iter().any(|p| p.username == "gone"));
        assert_eq!(people[2].tooltip(), "zoe · typing");
        assert_eq!(people[0].tooltip(), "Alice · owner · offline");
    }

    #[test]
    fn presence_updates_replace_the_previous_state() {
        let members = vec![member("bob", CollabMemberVia::Invite)];
        let mut live = HashMap::new();
        live.insert("bob".to_string(), CollabPresenceState::Active);
        live.insert("bob".to_string(), CollabPresenceState::Idle);
        let people = roster(&members, "alice", &live, None, &HashSet::new());
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].state, CollabPresenceState::Idle);
        assert!(!people[0].here());
    }
}
