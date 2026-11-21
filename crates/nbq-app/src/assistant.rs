use std::{path::PathBuf, sync::Arc};

use crate::{complete::Catalogs, query_tab::QueryTab, ui::Projects};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nbq_engine::{
    Engine,
    assistant::{Reply, Request, Skill},
};
pub struct Assistant {
    tab: WeakEntity<QueryTab>,
    engine: Arc<Engine>,
    catalogs: Catalogs,
    projects: Projects,
    input: Entity<InputState>,
    key: Entity<InputState>,
    settings: bool,
    busy: bool,
    revision: usize,
    history: Vec<(String, String)>,
    notice: String,
    skill: Option<(PathBuf, Skill, String)>,
    scroll: ScrollHandle,
    _enter: Subscription,
}

impl Assistant {
    pub fn new(
        tab: WeakEntity<QueryTab>,
        engine: Arc<Engine>,
        catalogs: Catalogs,
        projects: Projects,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask about your SQL…"));
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("OpenAI API key")
        });
        let enter = cx.subscribe_in(&input, window, |a: &mut Self, _, event, window, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
            if matches!(
                event,
                InputEvent::PressEnter {
                    secondary: false,
                    shift: false
                }
            ) {
                a.send(window, cx);
            }
        });
        Self {
            tab,
            engine,
            catalogs,
            projects,
            input,
            key,
            settings: false,
            busy: false,
            revision: 0,
            history: vec![],
            notice: String::new(),
            skill: None,
            scroll: ScrollHandle::new(),
            _enter: enter,
        }
    }

    pub fn in_use(&self, cx: &App) -> bool {
        self.busy || !self.history.is_empty() || !self.input.read(cx).value().is_empty()
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.revision += 1;
        self.busy = false;
        self.history.clear();
        self.notice.clear();
        self.skill = None;
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() || self.busy || self.skill.is_some() {
            return;
        }
        let Some(tab) = self.tab.upgrade() else {
            return;
        };
        let editor = tab.read(cx).editor.clone();
        let selection = editor.read(cx).selected_range();
        let script = editor.read(cx).value().to_string();
        let selected_sql = (!selection.is_empty()).then(|| script[selection].to_string());
        let tab = tab.read(cx);
        let conn = tab.conn.as_ref();
        let root = self
            .projects
            .borrow()
            .iter()
            .find(|p| Some(p.key) == tab.project)
            .map(|p| p.root.clone());
        let request = Request {
            database: conn
                .map(|c| format!("{:?} / {} / {}", c.kind, c.name, c.database))
                .unwrap_or_else(|| "No database selected".into()),
            script,
            selected_sql,
            prompt: prompt.clone(),
            history: self
                .history
                .iter()
                .rev()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            root: root.clone(),
        };
        let cached = conn.and_then(|c| self.catalogs.borrow().get(&c.key()).cloned());
        let (mut activity, job) = self.engine.ask_assistant(
            request,
            conn.filter(|_| tab.session_enabled && cached.is_none())
                .map(|c| (c.key(), crate::ui::params_for(c))),
            cached,
        );
        self.busy = true;
        self.notice.clear();
        self.history.push(("You".into(), prompt.clone()));
        self.input.update(cx, |s, cx| s.set_value("", window, cx));
        let revision = self.revision;
        cx.spawn_in(window, async move |this, cx| {
            while let Some(status) = activity.recv().await {
                let active = this.update(cx, |a, cx| {
                    if a.revision != revision {
                        return false;
                    }
                    a.notice = status;
                    cx.notify();
                    true
                });
                if !matches!(active, Ok(true)) {
                    return;
                }
            }
            let reply = job
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| format!("{e:#}")));
            let _ = this.update_in(cx, |a, window, cx| {
                if a.revision != revision {
                    return;
                }
                a.busy = false;
                a.notice.clear();
                match reply {
                    Ok(reply) => a.receive(reply, root, window, cx),
                    Err(e) => {
                        a.history.pop();
                        a.notice = e;
                        if a.input.read(cx).value().is_empty() {
                            a.input.update(cx, |s, cx| s.set_value(prompt, window, cx));
                        }
                    }
                }
                a.scroll.scroll_to_bottom();
                cx.notify();
            });
        })
        .detach();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn receive(
        &mut self,
        reply: Reply,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.push(("Assistant".into(), reply.message));
        if let Some(sql) = reply.sql.filter(|s| !s.trim().is_empty()) {
            self.append_sql(&sql, window, cx);
        }
        if let (Some(root), Some(skill)) = (root, reply.skill) {
            let diff = similar::TextDiff::from_lines(
                skill.original.as_deref().unwrap_or(""),
                &skill.markdown,
            )
            .unified_diff()
            .context_radius(3)
            .to_string();
            self.skill = Some((root, skill, diff));
        }
    }

    fn append_sql(&self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab.upgrade() else {
            return;
        };
        let editor = tab.read(cx).editor.clone();
        let script = editor.read(cx).value();
        let end = script.len();
        let prefix =
            if script.is_empty() || script.ends_with("\n\n") || script.ends_with("\r\n\r\n") {
                ""
            } else if script.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
        let start = end + prefix.len();
        let selection = start..start + sql.len();
        let focus = self.input.focus_handle(cx).is_focused(window);
        editor.update(cx, |e, cx| {
            e.set_selected_range(end..end, cx);
            e.replace(format!("{prefix}{sql}"), window, cx);
            e.set_selected_range(selection, cx);
            if focus {
                e.focus(window, cx);
            }
        });
        tab.update(cx, |t, cx| {
            t.dirty = true;
            cx.notify();
        });
    }

    fn save_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.key.read(cx).value().to_string();
        self.key.update(cx, |s, cx| s.set_value("", window, cx));
        let job = cx
            .background_executor()
            .spawn(async move { nbq_engine::assistant::save_key(key) });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |a, cx| {
                match result {
                    Ok(()) => {
                        a.settings = false;
                        a.notice = "API key saved.".into();
                    }
                    Err(e) => a.notice = e.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn accept_skill(&mut self, cx: &mut Context<Self>) {
        let Some((root, skill, _)) = self.skill.as_ref() else {
            return;
        };
        let (root, skill) = (root.clone(), skill.clone());
        let revision = self.revision;
        self.busy = true;
        let job = cx
            .background_executor()
            .spawn(async move { skill.accept(&root) });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |a, cx| {
                if a.revision != revision {
                    return;
                }
                a.busy = false;
                match result {
                    Ok(()) => {
                        a.skill = None;
                        a.notice = "Project skill saved.".into();
                    }
                    Err(e) => a.notice = e.to_string(),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Assistant {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let context = self
            .tab
            .upgrade()
            .and_then(|t| t.read(cx).conn.as_ref().map(|c| c.name.clone()))
            .unwrap_or_else(|| "Select a connection for schema lookup".into());
        let heading = |label: &'static str| {
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
        };
        let mut chat = v_flex()
            .id("assistant-history")
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_3()
            .gap_3();
        if self.history.is_empty() {
            chat = chat
                .child(div().text_sm().text_color(theme.muted_foreground).child(
                    "Highlight SQL to refer to it. New queries are added at the end of this tab.",
                ))
                .child(
                    h_flex().gap_2().flex_wrap().children(
                        [
                            ("draft-query", "Write a query", "Write a query that "),
                            (
                                "explain-query",
                                "Explain selection",
                                "Explain the selected SQL.",
                            ),
                        ]
                        .map(|(id, label, prompt)| {
                            Button::new(id)
                                .outline()
                                .xsmall()
                                .label(label)
                                .on_click(cx.listener(move |a, _, w, cx| {
                                    a.input.update(cx, |s, cx| {
                                        s.set_value(prompt, w, cx);
                                        s.focus(w, cx);
                                    })
                                }))
                        }),
                    ),
                );
        }
        for (role, msg) in &self.history {
            chat = chat.child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child(role.clone()),
                    )
                    .child(div().text_sm().child(msg.clone())),
            );
        }
        if let Some((_, skill, diff)) = &self.skill {
            chat = chat
                .child(heading("Suggested skill update"))
                .child(format!("skills/{}", skill.relative_path()))
                .child(div().text_xs().child(diff.clone()))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("accept-skill")
                                .primary()
                                .small()
                                .label("Accept")
                                .disabled(self.busy)
                                .on_click(cx.listener(|a, _, _, cx| a.accept_skill(cx))),
                        )
                        .child(
                            Button::new("dismiss-skill")
                                .ghost()
                                .small()
                                .label("Dismiss")
                                .disabled(self.busy)
                                .on_click(cx.listener(|a, _, _, cx| {
                                    a.skill = None;
                                    cx.notify();
                                })),
                        ),
                );
        }
        v_flex()
            .key_context("Assistant")
            .size_full()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .px_3()
                    .h(px(32.))
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(heading("SQL assistant"))
                    .child(
                        Button::new("key-settings")
                            .outline()
                            .xsmall()
                            .icon(gpui_kit::component::Icon::new(
                                gpui_kit::assets::IconName::KeyRound,
                            ))
                            .label("API key")
                            .on_click(cx.listener(|a, _, _, cx| {
                                a.settings = !a.settings;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Database)
                            .xsmall(),
                    )
                    .child(context),
            )
            .when(self.settings, |panel| {
                panel.child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(Input::new(&self.key)))
                        .child(
                            Button::new("save-key")
                                .small()
                                .label("Save key")
                                .on_click(cx.listener(|a, _, w, cx| a.save_key(w, cx))),
                        ),
                )
            })
            .child(chat)
            .when(!self.notice.is_empty(), |el| {
                el.child(div().px_3().py_2().text_sm().child(self.notice.clone()))
            })
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.input))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Enter to send"),
                            )
                            .child(
                                Button::new("send-chat")
                                    .outline()
                                    .small()
                                    .label(if self.busy { "Working…" } else { "Send" })
                                    .disabled(
                                        self.input.read(cx).value().trim().is_empty()
                                            || self.busy
                                            || self.skill.is_some(),
                                    )
                                    .on_click(cx.listener(|a, _, w, cx| a.send(w, cx))),
                            ),
                    ),
            )
    }
}
