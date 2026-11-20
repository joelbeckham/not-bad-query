use std::{ops::Range, path::PathBuf, sync::Arc};

use crate::{complete::Catalogs, query_tab::QueryTab, ui::Projects};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{EditorState, Input, InputEditsEvent, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use nbq_engine::{
    Engine,
    assistant::{Reply, Request, Skill},
};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
struct Target {
    sql: String,
    range: Rc<RefCell<Option<Range<usize>>>>,
    _edits: Rc<Subscription>,
}

impl Target {
    fn track(
        editor: &Entity<EditorState>,
        selection: Range<usize>,
        cx: &mut Context<Assistant>,
    ) -> Self {
        let sql = editor.read(cx).value()[selection.clone()].to_string();
        let range = Rc::new(RefCell::new(Some(selection)));
        let tracked = range.clone();
        let sub = cx.subscribe(editor, move |_, _, event: &InputEditsEvent, _| {
            for (edit, _) in event.edits() {
                let mut slot = tracked.borrow_mut();
                *slot = slot.take().and_then(|r| {
                    shift_target(r, edit.start_byte..edit.old_end_byte, edit.new_end_byte)
                });
            }
        });
        Self {
            sql,
            range,
            _edits: Rc::new(sub),
        }
    }
    fn resolve(&self, current: &str) -> Option<Range<usize>> {
        self.range
            .borrow()
            .clone()
            .filter(|r| current.get(r.clone()) == Some(self.sql.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::shift_target;

    #[test]
    fn editor_changes_move_or_invalidate_the_original_byte_range() {
        assert_eq!(shift_target(10..18, 2..4, 10), Some(16..24));
        assert_eq!(shift_target(10..18, 18..20, 19), Some(10..18));
        assert_eq!(shift_target(10..18, 13..13, 15), None);
        assert_eq!(shift_target(10..18, 0..20, 0), None);
    }
}

fn shift_target(range: Range<usize>, edit: Range<usize>, new_end: usize) -> Option<Range<usize>> {
    if edit.end <= range.start {
        Some(range.start - edit.end + new_end..range.end - edit.end + new_end)
    } else if edit.start >= range.end {
        Some(range)
    } else {
        None
    }
}

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
    last: Option<Target>,
    pending_sql: Option<String>,
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
            last: None,
            pending_sql: None,
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
        self.last = None;
        self.pending_sql = None;
        self.skill = None;
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.input.read(cx).value().trim().to_string();
        if prompt.is_empty() || self.busy || self.skill.is_some() || self.pending_sql.is_some() {
            return;
        }
        let Some(tab) = self.tab.upgrade() else {
            return;
        };
        let editor = tab.read(cx).editor.clone();
        let selection = editor.read(cx).selected_range();
        let target = if selection.is_empty() {
            self.last.clone()
        } else {
            Some(Target::track(&editor, selection, cx))
        };
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
            target: target.as_ref().map(|t| t.sql.clone()),
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
                    Ok(reply) => a.receive(reply, target, root, window, cx),
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
        target: Option<Target>,
        root: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.push(("Assistant".into(), reply.message));
        if let Some(sql) = reply.sql.filter(|s| !s.trim().is_empty()) {
            let target = target.filter(|_| !reply.new_query);
            if !self.apply(&sql, target.as_ref(), window, cx) {
                self.pending_sql = Some(sql);
                self.notice="The original query changed. Select the SQL to replace, or insert the proposal at the end.".into();
            }
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

    fn apply(
        &mut self,
        sql: &str,
        target: Option<&Target>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.tab.upgrade() else {
            return false;
        };
        let editor = tab.read(cx).editor.clone();
        let script = editor.read(cx).value().to_string();
        let range = match target {
            Some(t) => match t.resolve(&script) {
                Some(range) => range,
                None => return false,
            },
            None => script.len()..script.len(),
        };
        let prefix = if target.is_none() && !script.is_empty() && !script.ends_with('\n') {
            "\n\n"
        } else {
            ""
        };
        let start = range.start + prefix.len();
        let selection = start..start + sql.len();
        let focus = self.input.focus_handle(cx).is_focused(window);
        editor.update(cx, |e, cx| {
            e.set_selected_range(range, cx);
            e.replace(format!("{prefix}{sql}"), window, cx);
            e.set_selected_range(selection.clone(), cx);
            if focus {
                e.focus(window, cx);
            }
        });
        tab.update(cx, |t, cx| {
            t.dirty = true;
            cx.notify();
        });
        self.last = Some(Target::track(&editor, selection, cx));
        true
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
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Select SQL to explain or revise it, or start a new query."),
                )
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
        if let Some(sql) = self.pending_sql.clone() {
            chat = chat.child(div().text_sm().child(sql)).child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("apply-sql")
                            .small()
                            .label("Apply to editor")
                            .on_click(cx.listener(|a, _, w, cx| {
                                if let Some(sql) = a.pending_sql.take() {
                                    let target = a.tab.upgrade().and_then(|t| {
                                        let editor = t.read(cx).editor.clone();
                                        let range = editor.read(cx).selected_range();
                                        (!range.is_empty())
                                            .then(|| Target::track(&editor, range, cx))
                                    });
                                    a.apply(&sql, target.as_ref(), w, cx);
                                    a.notice.clear();
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("discard-sql")
                            .ghost()
                            .small()
                            .label("Dismiss")
                            .on_click(cx.listener(|a, _, _, cx| {
                                a.pending_sql = None;
                                a.notice.clear();
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some((_, skill, diff)) = &self.skill {
            chat = chat
                .child(heading("Suggested skill update"))
                .child(format!("skills/{}", skill.name))
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
                                            || self.skill.is_some()
                                            || self.pending_sql.is_some(),
                                    )
                                    .on_click(cx.listener(|a, _, w, cx| a.send(w, cx))),
                            ),
                    ),
            )
    }
}
