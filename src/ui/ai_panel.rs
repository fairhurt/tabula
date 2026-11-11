//! The Ask AI panel: chat with Claude Code or Copilot CLI about the current
//! connection. The agent runs on your own subscription with only Tabula's
//! tools, and SQL it suggests can go straight into the editor.

use gpui_kit::component::{
    StyledExt as _,
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div, px,
};
use futures::StreamExt as _;

use super::icon;
use crate::agents::Agent;
use crate::ai::{self, AiEvent, AskRequest, Run};

pub enum AiPanelEvent {
    /// Put this SQL in the active query tab.
    Insert(String),
    /// Put this SQL in the active query tab and run it.
    Run(String),
}

struct Msg {
    user: bool,
    text: String,
    tools: Vec<(String, String)>,
    error: Option<String>,
}

pub struct AiPanel {
    agent: Agent,
    installed: Vec<Agent>,
    /// Connection name and database the chat is about.
    target: Option<(String, String)>,
    messages: Vec<Msg>,
    input: Entity<InputState>,
    run: Option<Run>,
    /// Claude Code session, to continue the conversation.
    session: Option<String>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<AiPanelEvent> for AiPanel {}

impl AiPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Ask about this database, or describe the query you need…")
        });
        let sub = cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.send_input(window, cx);
            }
        });
        let installed: Vec<Agent> = Agent::ALL.into_iter().filter(|a| a.installed()).collect();
        AiPanel {
            agent: installed.first().copied().unwrap_or(Agent::Claude),
            installed,
            target: None,
            messages: vec![],
            input,
            run: None,
            session: None,
            scroll: ScrollHandle::new(),
            _subs: vec![sub],
        }
    }

    pub fn set_target(&mut self, connection: &str, database: &str, cx: &mut Context<Self>) {
        let t = Some((connection.to_string(), database.to_string()));
        if self.target != t {
            self.target = t;
            // A new database is a new conversation.
            self.session = None;
            self.messages.clear();
            cx.notify();
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.focus(window, cx));
    }

    fn send_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.update(cx, |i, cx| i.set_value("", window, cx));
        self.ask(text, cx);
    }


    /// Send `prompt` to the agent.
    pub fn ask(&mut self, prompt: String, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        let Some((connection, database)) = self.target.clone() else {
            self.messages.push(Msg {
                user: false,
                text: String::new(),
                tools: vec![],
                error: Some("Connect to a database first.".into()),
            });
            cx.notify();
            return;
        };
        let history = self
            .messages
            .chunks(2)
            .filter_map(|p| match p {
                [q, a] if q.user && !a.user => Some((q.text.clone(), a.text.clone())),
                _ => None,
            })
            .collect();
        self.messages.push(Msg {
            user: true,
            text: prompt.clone(),
            tools: vec![],
            error: None,
        });
        self.messages.push(Msg {
            user: false,
            text: String::new(),
            tools: vec![],
            error: None,
        });
        let (mut rx, run) = ai::ask(AskRequest {
            agent: self.agent,
            connection,
            database,
            prompt,
            history,
            resume: self.session.clone(),
        });
        self.run = Some(run);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let done = matches!(ev, AiEvent::Done { .. });
                let alive = this
                    .update(cx, |this, cx| {
                        if let Some(m) = this.messages.last_mut() {
                            match ev {
                                AiEvent::Text(t) => m.text.push_str(&t),
                                AiEvent::Tool { name, detail } => m.tools.push((name, detail)),
                                AiEvent::Done { session, error } => {
                                    if session.is_some() {
                                        this.session = session;
                                    }
                                    m.error = error;
                                    this.run = None;
                                }
                            }
                        }
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    })
                    .is_ok();
                if done || !alive {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.run.take() {
            r.stop();
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.messages.clear();
        self.session = None;
        cx.notify();
    }

    /// Show a finished exchange (headless screenshots).
    pub fn shot_conversation(&mut self, cx: &mut Context<Self>) {
        self.messages = vec![
            Msg {
                user: true,
                text: "Which customers placed the most orders last week?".into(),
                tools: vec![],
                error: None,
            },
            Msg {
                user: false,
                text: "Orders link to customers through `orders.customer_id`. Here are the top ten for the last 7 days:\n\n```sql\nSELECT c.name, c.email, count(*) AS orders\nFROM shop.orders o\nJOIN shop.customers c ON c.id = o.customer_id\nWHERE o.placed_at > now() - interval '7 days'\nGROUP BY c.id\nORDER BY orders DESC\nLIMIT 10;\n```\n\n`orders_placed_idx` covers the date filter, so this stays fast.".into(),
                tools: vec![
                    ("describe_table".into(), "shop.orders".into()),
                    ("run_query".into(), "select count(*) from shop.orders where placed_at > now() - interval '7 days'".into()),
                ],
                error: None,
            },
        ];
        cx.notify();
    }

    fn render_message(&self, ix: usize, m: &Msg, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        if m.user {
            return h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(320.))
                        .px_3()
                        .py_2()
                        .rounded(theme.radius_lg)
                        .bg(theme.primary.opacity(0.15))
                        .text_sm()
                        .child(m.text.clone()),
                )
                .into_any_element();
        }
        let running = self.run.is_some() && ix + 1 == self.messages.len();
        let blocks = ai::sql_blocks(&m.text);
        v_flex()
            .gap_2()
            .children(m.tools.iter().enumerate().map(|(i, (name, detail))| {
                h_flex()
                    .id(SharedString::from(format!("tool-{ix}-{i}")))
                    .gap_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(icon("database-search").xsmall())
                    .child(div().font_medium().child(name.clone()))
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_family(theme.mono_font_family.clone())
                            .child(detail.lines().next().unwrap_or("").to_string()),
                    )
            }))
            .when(!m.text.is_empty(), |d| {
                d.child(
                    div().text_sm().child(TextView::markdown(
                        SharedString::from(format!("ai-msg-{ix}")),
                        m.text.clone(),
                    )),
                )
            })
            .when(running && m.text.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Spinner::new().xsmall())
                        .child(format!("{} is thinking…", self.agent.label())),
                )
            })
            .when(!running && !blocks.is_empty(), |d| {
                d.child(h_flex().gap_1().flex_wrap().children(blocks.into_iter().enumerate().flat_map(
                    |(bi, sql)| {
                        let label = if bi == 0 { String::new() } else { format!(" #{}", bi + 1) };
                        let s2 = sql.clone();
                        [
                            Button::new(SharedString::from(format!("ins-{ix}-{bi}")))
                                .xsmall()
                                .outline()
                                .icon(icon("square-pen"))
                                .label(format!("Insert{label}"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(AiPanelEvent::Insert(sql.clone()))
                                })),
                            Button::new(SharedString::from(format!("run-{ix}-{bi}")))
                                .xsmall()
                                .ghost()
                                .icon(icon("play"))
                                .label(format!("Run{label}"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(AiPanelEvent::Run(s2.clone()))
                                })),
                        ]
                    },
                )))
            })
            .children(m.error.clone().map(|e| {
                div()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(theme.danger.opacity(0.1))
                    .text_color(theme.danger)
                    .text_xs()
                    .child(e)
            }))
            .into_any_element()
    }
}

impl Render for AiPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let agents = Agent::ALL.map(|a| {
            let installed = self.installed.contains(&a);
            Button::new(SharedString::from(format!("agent-{a:?}")))
                .xsmall()
                .label(match a {
                    Agent::Claude => "Claude Code",
                    Agent::Copilot => "Copilot CLI",
                })
                .when(self.agent == a, |b| b.primary())
                .when(self.agent != a, |b| b.ghost())
                .when(!installed, |b| b.tooltip(a.install_hint()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.agent != a {
                        this.agent = a;
                        this.session = None;
                        cx.notify();
                    }
                }))
        });
        let messages: Vec<_> = self
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| self.render_message(i, m, cx).into_any_element())
            .collect();
        let empty = self.messages.is_empty();
        let not_installed = !self.installed.contains(&self.agent);
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(icon("sparkles").small().text_color(theme.primary))
                    .child(div().font_semibold().text_sm().child("Ask AI"))
                    .child(div().flex_1())
                    .child(
                        Button::new("clear-chat")
                            .xsmall()
                            .ghost()
                            .icon(icon("eraser"))
                            .tooltip("New conversation")
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    ),
            )
            .child(h_flex().px_3().py_1().gap_1().children(agents))
            .child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(match &self.target {
                        Some((c, d)) => format!("On {c} · {d} · uses your {} sign-in", self.agent.label()),
                        None => "Connect to a database to start".into(),
                    }),
            )
            .child(
                div()
                    .id("ai-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px_3()
                    .py_3()
                    .child(
                        v_flex()
                            .gap_4()
                            .when(empty, |d| {
                                d.child(
                                    v_flex()
                                        .gap_2()
                                        .pt_6()
                                        .items_center()
                                        .text_color(theme.muted_foreground)
                                        .text_sm()
                                        .child(icon("bot").large())
                                        .child("The agent can read your schema and run read-only queries.")
                                        .child(div().text_xs().child("Try: \"Which tables reference customers?\""))
                                        .when(not_installed, |d| {
                                            d.child(
                                                div()
                                                    .mt_2()
                                                    .p_2()
                                                    .rounded(theme.radius)
                                                    .bg(theme.warning.opacity(0.12))
                                                    .text_color(theme.warning)
                                                    .text_xs()
                                                    .child(format!(
                                                        "{} isn't installed. {}",
                                                        self.agent.label(),
                                                        self.agent.install_hint()
                                                    )),
                                            )
                                        }),
                                )
                            })
                            .children(messages),
                    ),
            )
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().flex_1().child(Input::new(&self.input).small()))
                    .map(|d| {
                        if self.run.is_some() {
                            d.child(
                                Button::new("stop-ai")
                                    .small()
                                    .danger()
                                    .icon(icon("square"))
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            )
                        } else {
                            d.child(
                                Button::new("send-ai")
                                    .small()
                                    .primary()
                                    .icon(icon("arrow-up"))
                                    .disabled(self.target.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.send_input(window, cx)
                                    })),
                            )
                        }
                    }),
            )
    }
}
