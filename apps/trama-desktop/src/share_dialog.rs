//! The Share dialog (⌘⇧S): access level, invites, members and the join link.
//!
//! [`ShareState`] is the dialog's state machine; every action returns the
//! protocol message to send, so it is tested without a window. The view
//! only owns the invite input and forwards to the [`CollabStore`].

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use trama_domain::ids::ThreadId;
use trama_protocol::{
    ClientMessage, CollabAccess, CollabMember, CollabMemberVia, SharedThreadSnapshot,
};

use crate::collab::{CollabStore, encode_join, valid_name};
use crate::presence;
use crate::theme::{self, Palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SharePhase {
    #[default]
    Closed,
    /// No collaboration account: the dialog explains how to sign in.
    SignedOut,
    /// Not shared yet; choosing access and invites, then Share.
    NotShared,
    /// Waiting for the thread's members.
    Loading,
    Ready,
}

#[derive(Debug, Clone, Default)]
pub struct ShareState {
    thread: Option<ThreadId>,
    pub title: String,
    pub phase: SharePhase,
    /// The chosen access; follows the server once shared.
    access: Option<CollabAccess>,
    /// Usernames to invite with the next request.
    pub chips: Vec<String>,
    /// Chips sent with the request in flight.
    submitted: Vec<String>,
    pub snapshot: Option<SharedThreadSnapshot>,
    /// Join string with the link token. The token is returned once, so
    /// this is the only time it can be copied.
    pub link: Option<String>,
    pub busy: bool,
    pub error: Option<String>,
}

impl ShareState {
    pub fn thread(&self) -> Option<&ThreadId> {
        self.thread.as_ref()
    }

    pub fn access(&self) -> CollabAccess {
        self.access.unwrap_or(CollabAccess::Invited)
    }

    /// Reset for `thread`. A shared thread loads its members first.
    pub fn open(
        &mut self,
        thread: ThreadId,
        title: String,
        linked: bool,
        signed_in: bool,
    ) -> Option<ClientMessage> {
        *self = Self {
            thread: Some(thread.clone()),
            title,
            ..Self::default()
        };
        if !signed_in {
            self.phase = SharePhase::SignedOut;
            return None;
        }
        if linked {
            self.phase = SharePhase::Loading;
            Some(ClientMessage::GetSharedThread { thread_id: thread })
        } else {
            self.phase = SharePhase::NotShared;
            None
        }
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// The owner manages access and members; before the first share the
    /// person sharing becomes the owner.
    pub fn is_owner(&self, me: Option<&str>) -> bool {
        match &self.snapshot {
            None => self.phase == SharePhase::NotShared,
            Some(snapshot) => me.is_some_and(|me| snapshot.owner.eq_ignore_ascii_case(me)),
        }
    }

    fn known(&self, name: &str) -> bool {
        self.chips
            .iter()
            .any(|chip| chip.eq_ignore_ascii_case(name))
            || self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .members
                    .iter()
                    .any(|member| member.username.eq_ignore_ascii_case(name))
            })
    }

    /// Add usernames (separated by spaces or commas) as invite chips.
    pub fn add_chips(&mut self, raw: &str, me: Option<&str>) -> Result<(), String> {
        let names: Vec<&str> = raw
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|name| !name.is_empty())
            .collect();
        if names.is_empty() {
            return Err("Type a username to invite.".into());
        }
        if let Some(bad) = names.iter().find(|name| !valid_name(name)) {
            return Err(format!(
                "\u{201c}{bad}\u{201d} is not a username: use letters, digits, `.`, `_`, `-` or `@`."
            ));
        }
        if let Some(own) = names
            .iter()
            .find(|name| me.is_some_and(|me| me.eq_ignore_ascii_case(name)))
        {
            return Err(format!("{own} is you."));
        }
        for name in names {
            if !self.known(name) {
                self.chips.push(name.to_string());
            }
        }
        self.error = None;
        Ok(())
    }

    pub fn remove_chip(&mut self, name: &str) {
        self.chips.retain(|chip| chip != name);
    }

    /// Pick an access level. Once shared, the owner's change is applied
    /// right away; switching back to invited revokes the link.
    pub fn set_access(&mut self, access: CollabAccess, me: Option<&str>) -> Option<ClientMessage> {
        if self.busy || !self.is_owner(me) || self.access() == access {
            return None;
        }
        match self.phase {
            SharePhase::NotShared => {
                self.access = Some(access);
                None
            }
            SharePhase::Ready => {
                self.busy = true;
                self.error = None;
                self.access = Some(access);
                Some(ClientMessage::ShareThread {
                    thread_id: self.thread.clone()?,
                    access: Some(access),
                    invite: Vec::new(),
                })
            }
            _ => None,
        }
    }

    /// Share (first time) or send the invite chips.
    pub fn submit(&mut self, me: Option<&str>) -> Option<ClientMessage> {
        if self.busy || !self.is_owner(me) {
            return None;
        }
        let access = match self.phase {
            SharePhase::NotShared => Some(self.access()),
            SharePhase::Ready if !self.chips.is_empty() => None,
            _ => return None,
        };
        self.busy = true;
        self.error = None;
        self.submitted = self.chips.clone();
        Some(ClientMessage::ShareThread {
            thread_id: self.thread.clone()?,
            access,
            invite: self.chips.clone(),
        })
    }

    /// A new link token; the previous link stops working.
    pub fn rotate_link(&mut self, me: Option<&str>) -> Option<ClientMessage> {
        if self.busy || self.phase != SharePhase::Ready || !self.is_owner(me) {
            return None;
        }
        self.busy = true;
        self.error = None;
        Some(ClientMessage::ShareThread {
            thread_id: self.thread.clone()?,
            access: Some(CollabAccess::Link),
            invite: Vec::new(),
        })
    }

    pub fn remove_member(&mut self, username: &str, me: Option<&str>) -> Option<ClientMessage> {
        let snapshot = self.snapshot.as_ref()?;
        if self.busy
            || !self.is_owner(me)
            || snapshot.owner.eq_ignore_ascii_case(username)
            || !snapshot
                .members
                .iter()
                .any(|member| member.username == username)
        {
            return None;
        }
        self.busy = true;
        self.error = None;
        Some(ClientMessage::RemoveSharedThreadMember {
            thread_id: self.thread.clone()?,
            username: username.to_string(),
        })
    }

    pub fn apply_snapshot(&mut self, snapshot: &SharedThreadSnapshot, server_url: Option<&str>) {
        if self.phase == SharePhase::Closed || self.thread.as_ref() != Some(&snapshot.thread_id) {
            return;
        }
        self.phase = SharePhase::Ready;
        self.busy = false;
        self.error = None;
        self.access = Some(snapshot.access);
        let sent = std::mem::take(&mut self.submitted);
        self.chips.retain(|chip| !sent.contains(chip));
        match (&snapshot.link_token, server_url) {
            (Some(token), Some(server)) => {
                self.link = Some(encode_join(server, &snapshot.collab_thread_id, Some(token)));
            }
            _ if snapshot.access == CollabAccess::Invited => self.link = None,
            _ => {}
        }
        self.snapshot = Some(snapshot.clone());
    }

    pub fn apply_error(&mut self, message: String) {
        if self.phase == SharePhase::Closed {
            return;
        }
        self.busy = false;
        self.submitted.clear();
        if self.phase == SharePhase::Loading {
            self.phase = SharePhase::NotShared;
        }
        if let Some(snapshot) = &self.snapshot {
            self.access = Some(snapshot.access);
        }
        self.error = Some(message);
    }

    /// For invited people: names the thread without a token.
    pub fn invite_link(&self, server_url: Option<&str>) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        Some(encode_join(server_url?, &snapshot.collab_thread_id, None))
    }
}

pub fn via_label(via: CollabMemberVia) -> &'static str {
    match via {
        CollabMemberVia::Owner => "Owner",
        CollabMemberVia::Invite => "Invited",
        CollabMemberVia::Link => "Joined with link",
    }
}

// -- view -----------------------------------------------------------------------

pub enum ShareDialogEvent {
    /// Open the collaboration account dialog.
    OpenAccount,
}

pub struct ShareDialog {
    store: Entity<CollabStore>,
    invite: Entity<InputState>,
    copied: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ShareDialogEvent> for ShareDialog {}

impl ShareDialog {
    pub fn new(store: Entity<CollabStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let invite =
            cx.new(|cx| InputState::new(window, cx).placeholder("Invite by username, e.g. bob"));
        let subscriptions = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&invite, window, |this, _, event, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.add_or_invite(window, cx);
                }
            }),
        ];
        Self {
            store,
            invite,
            copied: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copied = None;
        self.invite
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.invite.read(cx).focus_handle(cx).focus(window, cx);
    }

    /// Enter in the invite field: add the typed names as chips, or send
    /// the chips when the field is empty.
    fn add_or_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.invite.read(cx).value().to_string();
        if typed.trim().is_empty() {
            self.submit(cx);
            return;
        }
        let added = self.store.update(cx, |store, cx| {
            let me = store.username().map(str::to_string);
            let result = store.share.add_chips(&typed, me.as_deref());
            if let Err(message) = &result {
                store.share.error = Some(message.clone());
            }
            cx.notify();
            result.is_ok()
        });
        if added {
            self.invite
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let me = store.username().map(str::to_string);
            let message = store.share.submit(me.as_deref());
            store.dispatch_share(message, cx);
        });
    }

    fn act(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut ShareState, Option<&str>) -> Option<ClientMessage>,
    ) {
        self.store.update(cx, |store, cx| {
            let me = store.username().map(str::to_string);
            let message = action(&mut store.share, me.as_deref());
            store.dispatch_share(message, cx);
        });
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        self.copied = Some(text);
        cx.notify();
    }
}

impl Render for ShareDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let store = self.store.read(cx);
        let state = store.share.clone();
        let me = store.username().map(str::to_string);
        let server = store.server_url().map(str::to_string);
        let presence = state
            .thread()
            .and_then(|id| store.presence(id).cloned())
            .unwrap_or_default();
        let owner = state.is_owner(me.as_deref());

        let header = h_flex()
            .gap(theme::sp(1.5))
            .items_center()
            .text_size(theme::text_sm())
            .text_color(p.text_muted)
            .child(Icon::new(IconName::MessageSquare).with_size(px(13.)))
            .child(div().min_w_0().truncate().child(state.title.clone()));

        if state.phase == SharePhase::SignedOut {
            return v_flex()
                .gap(theme::sp(3.))
                .child(header)
                .child(
                    v_flex()
                        .p(theme::sp(4.))
                        .gap(theme::sp(2.))
                        .items_center()
                        .rounded(theme::radius_lg())
                        .border_1()
                        .border_color(p.border)
                        .bg(p.panel)
                        .child(
                            Icon::new(IconName::Users)
                                .with_size(px(20.))
                                .text_color(p.text_muted),
                        )
                        .child(
                            div()
                                .text_size(theme::text_ui())
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Sign in to share threads"),
                        )
                        .child(
                            div()
                                .max_w(px(380.))
                                .text_center()
                                .text_size(theme::text_sm())
                                .line_height(relative(1.5))
                                .text_color(p.text_muted)
                                .child("Sharing goes through a collaboration server. Connect this daemon with your username and password, then share. Each person's turns still run on their own machine."),
                        )
                        .child(
                            Button::new("share-connect")
                                .primary()
                                .small()
                                .label("Connect to collaboration server…")
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(ShareDialogEvent::OpenAccount);
                                })),
                        ),
                )
                .into_any_element();
        }

        let access = state.access();
        let can_manage = owner && !state.busy && state.phase != SharePhase::Loading;
        let access_option = |id: &'static str,
                             value: CollabAccess,
                             icon: IconName,
                             title: &'static str,
                             body: &'static str,
                             cx: &mut Context<Self>| {
            let selected = access == value;
            h_flex()
                .id(id)
                .w_full()
                .px(theme::sp(3.))
                .py(theme::sp(2.))
                .gap(theme::sp(3.))
                .items_center()
                .rounded(theme::radius())
                .border_1()
                .border_color(if selected {
                    p.accent.opacity(0.45)
                } else {
                    p.border
                })
                .when(selected, |this| this.bg(p.accent_soft))
                .when(can_manage && !selected, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(p.hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.act(cx, |state, me| state.set_access(value, me));
                        }))
                })
                .child(Icon::new(icon).with_size(px(15.)).text_color(if selected {
                    p.accent_text
                } else {
                    p.text_muted
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(theme::text_ui())
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(theme::text_xs())
                                .text_color(p.text_muted)
                                .child(body),
                        ),
                )
                .when(selected, |this| {
                    this.child(
                        Icon::new(IconName::Check)
                            .with_size(px(13.))
                            .text_color(p.accent_text),
                    )
                })
        };
        let access_section = section(
            &p,
            "General access",
            v_flex()
                .gap(theme::sp(1.5))
                .child(access_option(
                    "share-access-invited",
                    CollabAccess::Invited,
                    IconName::Lock,
                    "Only invited people",
                    "You and the people you invite can open this thread.",
                    cx,
                ))
                .child(access_option(
                    "share-access-link",
                    CollabAccess::Link,
                    IconName::Link,
                    "Anyone with the link",
                    "Anyone signed in to this server who has the link can join.",
                    cx,
                )),
        );

        let chips = state.chips.clone();
        let invite_section = section(
            &p,
            "Invite people",
            v_flex()
                .gap(theme::sp(2.))
                .child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.invite).id("share-invite").small()),
                        )
                        .child(
                            Button::new("share-add-invite")
                                .small()
                                .icon(IconName::UserPlus)
                                .label("Add")
                                .disabled(!owner || state.busy)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_or_invite(window, cx);
                                })),
                        ),
                )
                .when(!chips.is_empty(), |this| {
                    this.child(h_flex().flex_wrap().gap(theme::sp(1.5)).children(
                        chips.into_iter().map(|name| {
                            let remove = name.clone();
                            h_flex()
                                .h(px(24.))
                                .pl(px(3.))
                                .pr(theme::sp(1.))
                                .gap(theme::sp(1.5))
                                .items_center()
                                .rounded_full()
                                .border_1()
                                .border_color(p.accent.opacity(0.35))
                                .bg(p.accent_soft)
                                .text_size(theme::text_xs())
                                .child(presence::avatar(&p, &name, 18., false))
                                .child(name.clone())
                                .child(
                                    div()
                                        .id(SharedString::from(format!("share-chip-{name}")))
                                        .size(px(16.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_full()
                                        .cursor_pointer()
                                        .hover(|style| style.bg(p.hover))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let remove = remove.clone();
                                            this.store.update(cx, |store, cx| {
                                                store.share.remove_chip(&remove);
                                                cx.notify();
                                            });
                                        }))
                                        .child(
                                            Icon::new(IconName::Close)
                                                .with_size(px(10.))
                                                .text_color(p.text_faint),
                                        ),
                                )
                        }),
                    ))
                }),
        );

        let members_section = state.snapshot.clone().map(|snapshot| {
            let mut members = snapshot.members.clone();
            members.sort_by_key(|member| {
                (
                    member.via != CollabMemberVia::Owner,
                    member.username.to_lowercase(),
                )
            });
            let can_remove = owner && !state.busy;
            let mut rows = Vec::with_capacity(members.len());
            for member in &members {
                rows.push(self.member_row(
                    member,
                    &snapshot,
                    me.as_deref(),
                    &presence,
                    can_remove,
                    &p,
                    cx,
                ));
            }
            section(
                &p,
                "People with access",
                v_flex().gap(px(2.)).children(rows),
            )
        });

        let link_section = self.link_section(&state, server.as_deref(), owner, &p, cx);

        let (primary_label, primary_enabled) = match state.phase {
            SharePhase::NotShared => ("Share", owner && !state.busy),
            _ => (
                "Send invites",
                owner && !state.busy && !state.chips.is_empty(),
            ),
        };

        v_flex()
            .gap(theme::sp(4.))
            .child(header)
            .when(state.phase == SharePhase::Loading, |this| {
                this.child(
                    h_flex()
                        .gap(theme::sp(2.))
                        .items_center()
                        .text_size(theme::text_sm())
                        .text_color(p.text_muted)
                        .child(Spinner::new().small().color(p.accent))
                        .child("Loading members…"),
                )
            })
            .child(access_section)
            .when(owner, |this| this.child(invite_section))
            .children(members_section)
            .children(link_section)
            .when_some(state.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_size(theme::text_sm())
                        .text_color(p.danger)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .gap(theme::sp(2.))
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(Icon::new(IconName::Info).with_size(px(12.)))
                            .child(
                                div()
                                    .min_w_0()
                                    .child("Each person's turns run on their own machine with their own Claude Code."),
                            ),
                    )
                    .when(state.busy, |this| {
                        this.child(Spinner::new().small().color(p.accent))
                    })
                    .when(owner, |this| {
                        this.child(
                            Button::new("share-submit")
                                .primary()
                                .small()
                                .label(primary_label)
                                .disabled(!primary_enabled)
                                .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl ShareDialog {
    #[allow(clippy::too_many_arguments)]
    fn member_row(
        &self,
        member: &CollabMember,
        snapshot: &SharedThreadSnapshot,
        me: Option<&str>,
        presence: &std::collections::HashMap<String, trama_protocol::CollabPresenceState>,
        can_remove: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_me = me.is_some_and(|me| me.eq_ignore_ascii_case(&member.username));
        let is_owner = snapshot.owner.eq_ignore_ascii_case(&member.username);
        let state = presence
            .get(&member.username.to_lowercase())
            .copied()
            .unwrap_or(trama_protocol::CollabPresenceState::Offline);
        let online = is_me || presence::is_here(state);
        let name = member.username.clone();
        h_flex()
            .h(px(36.))
            .px(theme::sp(2.))
            .gap(theme::sp(2.5))
            .items_center()
            .rounded(theme::radius())
            .hover(|style| style.bg(p.hover))
            .child(presence::avatar(p, &member.username, 24., !online))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap(theme::sp(1.5))
                            .items_center()
                            .text_size(theme::text_ui())
                            .child(member.username.clone())
                            .when(is_me, |this| {
                                this.child(div().text_color(p.text_faint).child("(you)"))
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child(format!(
                                "{} · {}",
                                via_label(member.via),
                                if is_me {
                                    "here"
                                } else {
                                    presence::state_label(state)
                                }
                            )),
                    ),
            )
            .when(is_owner, |this| this.child(presence::owner_badge(p)))
            .when(can_remove && !is_owner, |this| {
                this.child(
                    Button::new(SharedString::from(format!("share-remove-{name}")))
                        .ghost()
                        .xsmall()
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let name = name.clone();
                            this.act(cx, move |state, me| state.remove_member(&name, me));
                        })),
                )
            })
            .into_any_element()
    }

    fn link_section(
        &self,
        state: &ShareState,
        server: Option<&str>,
        owner: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if state.phase != SharePhase::Ready {
            return None;
        }
        let link_box = |text: String, id: &'static str, cx: &mut Context<Self>| {
            let copied = self.copied.as_deref() == Some(text.as_str());
            let copy = text.clone();
            h_flex()
                .w_full()
                .gap(theme::sp(2.))
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(28.))
                        .px(theme::sp(2.))
                        .flex()
                        .items_center()
                        .rounded(theme::radius())
                        .border_1()
                        .border_color(p.border)
                        .bg(p.panel)
                        .font_family(theme::MONO_FONT)
                        .text_size(theme::text_xs())
                        .text_color(p.text_muted)
                        .child(div().min_w_0().truncate().child(text)),
                )
                .child(
                    Button::new(id)
                        .small()
                        .icon(if copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .label(if copied { "Copied" } else { "Copy link" })
                        .on_click(cx.listener(move |this, _, _, cx| this.copy(copy.clone(), cx))),
                )
        };
        let body = match (&state.link, state.access()) {
            (Some(link), _) => v_flex()
                .gap(theme::sp(2.))
                .child(link_box(link.clone(), "share-copy-link", cx))
                .child(
                    h_flex()
                        .px(theme::sp(2.5))
                        .py(theme::sp(2.))
                        .gap(theme::sp(2.))
                        .items_start()
                        .rounded(theme::radius())
                        .bg(p.warning.opacity(0.10))
                        .border_1()
                        .border_color(p.warning.opacity(0.30))
                        .text_size(theme::text_xs())
                        .line_height(relative(1.45))
                        .text_color(p.text)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .with_size(px(13.))
                                .text_color(p.warning),
                        )
                        .child(div().flex_1().child(
                            "Copy this link now: it is shown only once and is not stored. Anyone signed in to this server who has it can join. Making a new link turns this one off.",
                        )),
                )
                .into_any_element(),
            (None, CollabAccess::Link) => h_flex()
                .gap(theme::sp(2.))
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(theme::text_sm())
                        .text_color(p.text_muted)
                        .child("Link access is on. The link was shown once when it was made; make a new one to share it again."),
                )
                .when(owner, |this| {
                    this.child(
                        Button::new("share-rotate-link")
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("New link")
                            .disabled(state.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.act(cx, |state, me| state.rotate_link(me));
                            })),
                    )
                })
                .into_any_element(),
            (None, CollabAccess::Invited) => {
                let invite = state.invite_link(server)?;
                v_flex()
                    .gap(theme::sp(1.5))
                    .child(link_box(invite, "share-copy-invite", cx))
                    .child(
                        div()
                            .text_size(theme::text_xs())
                            .text_color(p.text_faint)
                            .child("Only invited people can use this link. They can also open the thread from Shared with me."),
                    )
                    .into_any_element()
            }
        };
        Some(section(p, "Link", body).into_any_element())
    }
}

fn section(p: &Palette, title: &str, body: impl IntoElement) -> Div {
    v_flex()
        .gap(theme::sp(2.))
        .child(
            div()
                .text_size(theme::text_xs())
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_faint)
                .child(title.to_uppercase()),
        )
        .child(body)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientMessage, CollabAccess, CollabMember, CollabMemberVia, SharePhase, ShareState,
        SharedThreadSnapshot, ThreadId,
    };

    fn snapshot(access: CollabAccess, token: Option<&str>) -> SharedThreadSnapshot {
        SharedThreadSnapshot {
            thread_id: ThreadId::from("t"),
            collab_thread_id: "ct".into(),
            title: "Thread".into(),
            owner: "alice".into(),
            access,
            members: vec![
                CollabMember {
                    username: "alice".into(),
                    via: CollabMemberVia::Owner,
                },
                CollabMember {
                    username: "bob".into(),
                    via: CollabMemberVia::Invite,
                },
            ],
            link_token: token.map(str::to_string),
        }
    }

    const SERVER: Option<&str> = Some("http://127.0.0.1:8787");

    #[test]
    fn signed_out_dialog_sends_nothing() {
        let mut state = ShareState::default();
        assert_eq!(
            state.open(ThreadId::from("t"), "T".into(), false, false),
            None
        );
        assert_eq!(state.phase, SharePhase::SignedOut);
        assert_eq!(state.submit(Some("alice")), None);
    }

    #[test]
    fn first_share_sends_access_and_invites_then_shows_the_link_once() {
        let mut state = ShareState::default();
        assert_eq!(
            state.open(ThreadId::from("t"), "T".into(), false, true),
            None
        );
        assert_eq!(state.phase, SharePhase::NotShared);
        assert!(state.is_owner(Some("alice")));
        assert_eq!(state.set_access(CollabAccess::Link, Some("alice")), None);
        state.add_chips("bob, carol", Some("alice")).unwrap();
        assert_eq!(state.chips, vec!["bob", "carol"]);
        assert!(state.add_chips("alice", Some("alice")).is_err());
        assert!(state.add_chips("bad/name", Some("alice")).is_err());
        state.add_chips("BOB", Some("alice")).unwrap();
        assert_eq!(state.chips.len(), 2, "duplicates are ignored");

        let sent = state.submit(Some("alice"));
        assert_eq!(
            sent,
            Some(ClientMessage::ShareThread {
                thread_id: ThreadId::from("t"),
                access: Some(CollabAccess::Link),
                invite: vec!["bob".into(), "carol".into()],
            })
        );
        assert!(state.busy);
        assert_eq!(state.submit(Some("alice")), None, "one request at a time");

        state.apply_snapshot(&snapshot(CollabAccess::Link, Some(&"f".repeat(64))), SERVER);
        assert_eq!(state.phase, SharePhase::Ready);
        assert!(!state.busy);
        assert!(state.chips.is_empty());
        let link = state.link.clone().unwrap();
        assert_eq!(crate::collab::parse_join(&link).unwrap().thread_id, "ct");

        // A later refresh carries no token; the revealed link stays until close.
        state.apply_snapshot(&snapshot(CollabAccess::Link, None), SERVER);
        assert_eq!(state.link.as_deref(), Some(link.as_str()));
        state.close();
        assert_eq!(state.link, None);
        assert_eq!(state.phase, SharePhase::Closed);
    }

    #[test]
    fn shared_thread_loads_members_and_owner_actions_apply_at_once() {
        let mut state = ShareState::default();
        assert_eq!(
            state.open(ThreadId::from("t"), "T".into(), true, true),
            Some(ClientMessage::GetSharedThread {
                thread_id: ThreadId::from("t")
            })
        );
        assert_eq!(state.phase, SharePhase::Loading);
        state.apply_snapshot(&snapshot(CollabAccess::Invited, None), SERVER);
        assert!(state.invite_link(SERVER).is_some());

        // Not the owner: nothing to manage.
        assert_eq!(state.set_access(CollabAccess::Link, Some("bob")), None);
        assert_eq!(state.remove_member("alice", Some("bob")), None);

        assert_eq!(
            state.remove_member("alice", Some("alice")),
            None,
            "the owner stays"
        );
        assert_eq!(
            state.remove_member("bob", Some("alice")),
            Some(ClientMessage::RemoveSharedThreadMember {
                thread_id: ThreadId::from("t"),
                username: "bob".into(),
            })
        );
        state.apply_error("boom".into());
        assert!(!state.busy);
        assert_eq!(state.error.as_deref(), Some("boom"));

        assert_eq!(
            state.set_access(CollabAccess::Link, Some("alice")),
            Some(ClientMessage::ShareThread {
                thread_id: ThreadId::from("t"),
                access: Some(CollabAccess::Link),
                invite: Vec::new(),
            })
        );
        // Failure puts the server's access back.
        state.apply_error("nope".into());
        assert_eq!(state.access(), CollabAccess::Invited);

        // Snapshots of another thread are ignored.
        let mut other = snapshot(CollabAccess::Link, Some(&"a".repeat(64)));
        other.thread_id = ThreadId::from("other");
        state.apply_snapshot(&other, SERVER);
        assert_eq!(state.link, None);
    }

    #[test]
    fn failed_load_falls_back_to_not_shared_and_rotation_needs_ready() {
        let mut state = ShareState::default();
        state.open(ThreadId::from("t"), "T".into(), true, true);
        assert_eq!(state.rotate_link(Some("alice")), None);
        state.apply_error("not linked".into());
        assert_eq!(state.phase, SharePhase::NotShared);
        state.apply_snapshot(&snapshot(CollabAccess::Link, None), SERVER);
        assert!(state.rotate_link(Some("alice")).is_some());
    }
}
