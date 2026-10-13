use super::*;

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = theme::palette(cx);
        let link = self.sidebar.read(cx).onboarding.link().clone();
        let alert = self.sidebar.read(cx).onboarding.alert().map(str::to_string);
        let can_start = self.sidebar.read(cx).onboarding.can_start_daemon();
        let daemon_feedback = self.sidebar.read(cx).onboarding.daemon_feedback.clone();

        let online = matches!(link, Link::Online) || self.dev_demo;
        // The room the transcript and the side pane share.
        let viewport = f32::from(window.viewport_size().width);
        let drawer = if self.drawer_open {
            f32::from(theme::drawer_width())
        } else {
            0.
        };
        let available = viewport - f32::from(theme::sidebar_width()) - drawer;
        let pane = match (online, self.pane_info(cx)) {
            (true, Some(info)) => {
                let weak = cx.weak_entity();
                let actions = crate::side_pane::PaneActions {
                    back: Box::new({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| this.pane_back(cx));
                        }
                    }),
                    expand: Box::new({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.pane.toggle_expanded();
                                cx.notify();
                            });
                        }
                    }),
                    open_main: Box::new({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if let Some(thread) = this.pane.thread().cloned() {
                                    this.sidebar.update(cx, |sidebar, cx| {
                                        sidebar.select_thread(thread, cx)
                                    });
                                }
                            });
                        }
                    }),
                    close: Box::new({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| this.close_pane(cx));
                        }
                    }),
                    start_resize: Box::new(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.pane.resizing = true;
                            cx.notify();
                        });
                    }),
                };
                let running = self.pane_chat.read(cx).is_running();
                let width = (!self.pane.expanded).then(|| self.pane.shown_width(available));
                Some(crate::side_pane::render(
                    &self.pane,
                    &info,
                    running,
                    width,
                    self.pane_chat.clone().into(),
                    actions,
                    &p,
                ))
            }
            _ => None,
        };
        let center_hidden = pane.is_some() && self.pane.expanded;
        let center = if online {
            v_flex()
                .size_full()
                .child(self.subthread_bar.clone())
                .child(div().flex_1().min_h_0().child(self.chat.clone()))
                .into_any_element()
        } else {
            connection_panel(
                &p,
                matches!(link, Link::ProtocolMismatch { .. }),
                can_start,
                daemon_feedback.map(|feedback| feedback.text().to_string()),
                cx.listener(|this, _, _, cx| this.start_daemon(cx)),
            )
            .into_any_element()
        };

        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &keymap::NewThread, window, cx| {
                this.open_new_thread(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ToggleDrawer, _, cx| {
                this.toggle_drawer(cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::QuickSwitch, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ShowShortcuts, window, cx| {
                this.open_shortcuts(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ManageProjects, window, cx| {
                this.open_projects(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ShareThread, window, cx| {
                this.open_share(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ArchiveThread, window, cx| {
                this.archive_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::ToggleAppearance, _, cx| {
                this.toggle_appearance(cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::CloseOverlay, window, cx| {
                this.close_overlay(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keymap::SendToSubthread, window, cx| {
                    this.send_to_subthread(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &keymap::AllowOnce, _, cx| {
                this.answer_approval(ApprovalDecision::AllowOnce, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::AllowSession, _, cx| {
                this.answer_approval(ApprovalDecision::AllowSession, cx);
            }))
            .on_action(cx.listener(|this, _: &keymap::DenyApproval, _, cx| {
                this.answer_approval(ApprovalDecision::Deny, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keymap::CommentOnSelection, window, cx| {
                    this.comment_on_selection(window, cx);
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::terminal_view::ToggleTerminal, window, cx| {
                    this.chat
                        .update(cx, |chat, cx| chat.toggle_terminal(window, cx));
                },
            ))
            // Text inputs (the composer) handle Escape first; close the
            // drawer on the way down so Esc works with the composer focused.
            .capture_action(cx.listener(
                |this, _: &gpui_kit::component::input::Escape, window, cx| {
                    // An open diff comment composer takes Escape itself.
                    if this.drawer_open
                        && !window.has_active_dialog(cx)
                        && !this.changes.read(cx).has_draft()
                    {
                        this.drawer_open = false;
                        cx.notify();
                    }
                },
            ))
            .size_full()
            .bg(p.bg)
            .text_color(p.text)
            .font_family(theme::UI_FONT)
            .text_size(theme::text_ui())
            .child(self.render_title_bar(cx))
            .when_some(alert, |this, alert| {
                this.child(
                    h_flex()
                        .w_full()
                        .flex_shrink_0()
                        .px(theme::sp(3.))
                        .py(theme::sp(1.5))
                        .gap(theme::sp(2.))
                        .items_center()
                        .bg(p.danger.opacity(0.10))
                        .border_b_1()
                        .border_color(p.danger.opacity(0.25))
                        .text_size(theme::text_sm())
                        .text_color(p.text)
                        .child(
                            Icon::new(IconName::TriangleAlert)
                                .with_size(px(13.))
                                .text_color(p.danger),
                        )
                        .child(div().flex_1().child(alert)),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.pane.resizing, |this| {
                        this.cursor_col_resize()
                            .on_mouse_move(cx.listener(
                                move |this, event: &MouseMoveEvent, window, cx| {
                                    let right = f32::from(window.viewport_size().width) - drawer;
                                    this.pane
                                        .resize(right - f32::from(event.position.x), available);
                                    cx.notify();
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.pane.resizing = false;
                                    cx.notify();
                                }),
                            )
                    })
                    .child(
                        div()
                            .w(theme::sidebar_width())
                            .h_full()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(p.border)
                            .child(self.sidebar.clone()),
                    )
                    .when(!center_hidden, |this| {
                        this.child(div().flex_1().min_w_0().h_full().child(center))
                    })
                    .children(pane)
                    .when(self.drawer_open, |this| {
                        let body: AnyView = match self.drawer_tab {
                            DrawerTab::Changes => self.changes.clone().into(),
                            DrawerTab::Copies => self.copies.clone().into(),
                        };
                        let weak = cx.weak_entity();
                        this.child(render_drawer(
                            self.drawer_tab,
                            body,
                            move |tab, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.drawer_tab = tab;
                                    this.refresh_changes(cx);
                                    cx.notify();
                                });
                            },
                            cx.listener(|this, _, _, cx| this.toggle_drawer(cx)),
                            cx,
                        ))
                    }),
            )
            .child(self.render_status_bar(cx))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
