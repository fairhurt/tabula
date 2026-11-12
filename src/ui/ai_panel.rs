//! The Ask AI panel: chat with Claude Code or Copilot CLI about the current
//! connection. The agent runs on your own subscription with only Tabula's
//! tools, and SQL it suggests can go straight into the editor.

use gpui_kit::component::{
    StyledExt as _,
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonRounded, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    spinner::Spinner,
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, ScrollHandle, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window, div,
};
use futures::StreamExt as _;
use std::collections::HashMap;

use super::icon;
use crate::agents::Agent;
use crate::ai::{self, AiEvent, AskRequest, Run};

pub enum AiPanelEvent {
    /// Put this SQL in the active query tab.
    Insert(String),
    /// Put this SQL in the active query tab and run it.
    Run(String),
    /// The agent or a model choice changed; save them.
    Prefs,
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
    /// The agent's session, to continue the conversation.
    session: Option<String>,
    scroll: ScrollHandle,
    /// The model picked for each agent, by [`Agent::key`]. Absent: its default.
    models: HashMap<String, String>,
    /// Typing a model id that isn't in the list.
    custom_model: Entity<InputState>,
    /// The agent a model id is being typed for.
    typing_model: Option<Agent>,
    /// The models Copilot offers this account, as (name, id), once asked for.
    copilot_models: Vec<(String, String)>,
    asked_for_models: bool,
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
        let custom_model = cx.new(|cx| InputState::new(window, cx).placeholder("Model id, then Enter (Esc to cancel)"));
        let sub2 = cx.subscribe_in(&custom_model, window, |this, input, e: &InputEvent, _, cx| match e {
            InputEvent::PressEnter { .. } => {
                let id = input.read(cx).value().trim().to_string();
                if let (Some(agent), false) = (this.typing_model.take(), id.is_empty()) {
                    this.choose(agent, Some(id), cx);
                }
                cx.notify();
            }
            InputEvent::Blur => {
                this.typing_model = None;
                cx.notify();
            }
            _ => {}
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
            models: HashMap::new(),
            custom_model,
            typing_model: None,
            copilot_models: vec![],
            asked_for_models: false,
            _subs: vec![sub, sub2],
        }
    }

    /// Restore the saved agent and model choices.
    pub fn set_prefs(&mut self, agent: Option<&str>, models: HashMap<String, String>) {
        if let Some(a) = agent.and_then(Agent::from_key) {
            if self.installed.contains(&a) || self.installed.is_empty() {
                self.agent = a;
            }
        }
        self.models = models;
    }

    /// The agent in use and every model choice, for saving.
    pub fn prefs(&self) -> (String, HashMap<String, String>) {
        (self.agent.key().to_string(), self.models.clone())
    }

    fn model(&self) -> Option<&str> {
        self.models.get(self.agent.key()).map(String::as_str)
    }

    /// Ask Copilot which models this account can use, once. It starts the
    /// Copilot server, so it waits until the panel is actually shown.
    fn load_copilot_models(&mut self, cx: &mut Context<Self>) {
        if self.asked_for_models || !self.installed.contains(&Agent::Copilot) {
            return;
        }
        self.asked_for_models = true;
        let fut = crate::rt::spawn(crate::copilot::models());
        cx.spawn(async move |this, cx| {
            // On failure the short built-in list stays.
            if let Ok(models) = fut.await {
                let _ = this.update(cx, |this, cx| {
                    this.copilot_models = models;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The models to offer for `agent`, as (label, id).
    fn models_of(&self, agent: Agent) -> Vec<(String, String)> {
        let mut list: Vec<(String, String)> =
            agent.models().iter().map(|(l, id)| (l.to_string(), id.to_string())).collect();
        if agent == Agent::Copilot {
            for (name, id) in &self.copilot_models {
                if !list.iter().any(|(_, known)| known == id) {
                    list.push((name.clone(), id.clone()));
                }
            }
        }
        list
    }

    /// Use `agent` with `model` (`None` is the agent's own default). The
    /// same agent carries the conversation on with the new model; another
    /// agent starts its own session, with the earlier turns still shown.
    fn choose(&mut self, agent: Agent, model: Option<String>, cx: &mut Context<Self>) {
        if self.run.is_some() {
            return;
        }
        if self.agent != agent {
            self.agent = agent;
            self.session = None;
        }
        self.typing_model = None;
        match model {
            Some(m) => self.models.insert(agent.key().to_string(), m),
            None => self.models.remove(agent.key()),
        };
        cx.emit(AiPanelEvent::Prefs);
        cx.notify();
    }

    /// Show the field for typing a model id for `agent`.
    fn type_model(&mut self, agent: Agent, window: &mut Window, cx: &mut Context<Self>) {
        self.typing_model = Some(agent);
        let current = self.models.get(agent.key()).cloned().unwrap_or_default();
        self.custom_model.update(cx, |i, cx| {
            i.set_value(current, window, cx);
            i.focus(window, cx);
        });
        cx.notify();
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
            model: self.model().map(str::to_string),
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
                        .max_w(super::rem(320.))
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
        self.load_copilot_models(cx);
        let agent = self.agent;
        let current = self.model().map(str::to_string);
        let running = self.run.is_some();
        let offered: Vec<(Agent, Vec<(String, String)>)> =
            Agent::ALL.into_iter().map(|a| (a, self.models_of(a))).collect();
        let current_label = match &current {
            None => "Default".to_string(),
            Some(id) => offered
                .iter()
                .find(|(a, _)| *a == agent)
                .and_then(|(_, list)| list.iter().find(|(_, known)| known == id))
                .map_or_else(|| id.clone(), |(label, _)| label.clone()),
        };
        let picker = {
            let weak = cx.entity().downgrade();
            let installed = self.installed.clone();
            let models = self.models.clone();
            Button::new("ai-model")
                .small()
                .ghost()
                .rounded(ButtonRounded::Large)
                .icon(icon(agent_icon(agent)))
                .label(format!("{} · {current_label}", agent.short_label()))
                .tooltip("Choose the agent and model")
                .disabled(running)
                // Opens upwards: the prompt box sits at the bottom of the panel.
                .dropdown_menu_with_anchor(gpui_kit::Anchor::BottomLeft, move |mut menu, _, _| {
                    for (n, (a, list)) in offered.iter().enumerate() {
                        let a = *a;
                        let ready = installed.contains(&a);
                        let chosen = models.get(a.key()).cloned();
                        if n > 0 {
                            menu = menu.separator();
                        }
                        menu = menu.label(if ready {
                            a.label().to_string()
                        } else {
                            format!("{} · not installed", a.label())
                        });
                        // One click picks the agent and its model together.
                        let pick = |label: String, id: Option<String>| {
                            let weak = weak.clone();
                            PopupMenuItem::new(label)
                                .checked(a == agent && chosen == id)
                                .disabled(!ready)
                                .on_click(move |_, _, cx| {
                                    let _ = weak.update(cx, |this, cx| this.choose(a, id.clone(), cx));
                                })
                        };
                        menu = menu.item(pick("Default".into(), None));
                        for (label, id) in list {
                            menu = menu.item(pick(label.clone(), Some(id.clone())));
                        }
                        // A typed id that isn't in the list stays visible.
                        if let Some(c) = chosen.clone().filter(|c| !list.iter().any(|(_, id)| id == c)) {
                            menu = menu.item(pick(c.clone(), Some(c)));
                        }
                        let weak = weak.clone();
                        menu = menu.item(PopupMenuItem::new("Other model…").disabled(!ready).on_click(
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| this.type_model(a, window, cx));
                            },
                        ));
                    }
                    menu
                })
        };
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
            .child(
                div()
                    .px_3()
                    .pt_2()
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
                                        .child(
                                            v_flex().pt_2().gap_1().items_center().children(
                                                SUGGESTIONS.iter().enumerate().map(|(i, text)| {
                                                    Button::new(("suggest", i))
                                                        .small()
                                                        .outline()
                                                        .rounded(ButtonRounded::Large)
                                                        .label(*text)
                                                        .disabled(self.target.is_none())
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.ask(SUGGESTIONS[i].to_string(), cx)
                                                        }))
                                                }),
                                            ),
                                        )
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
                div().p_2().child(
                    v_flex()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(div().px_1().pt_1().child(Input::new(&self.input).appearance(false)))
                        .child(
                            h_flex()
                                .px_1()
                                .pb_1()
                                .gap_1()
                                .map(|d| match self.typing_model {
                                    // Typing a model id takes the picker's place.
                                    Some(a) => d
                                        .child(
                                            div()
                                                .pl_2()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format!("{} model", a.short_label())),
                                        )
                                        .child(div().flex_1().child(Input::new(&self.custom_model).xsmall()))
                                        .child(
                                            Button::new("model-cancel")
                                                .xsmall()
                                                .ghost()
                                                .icon(icon("x"))
                                                .tooltip("Keep the current model")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.typing_model = None;
                                                    cx.notify();
                                                })),
                                        ),
                                    None => d.child(picker).child(div().flex_1()),
                                })
                                .map(|d| {
                                    if running {
                                        d.child(
                                            Button::new("stop-ai")
                                                .small()
                                                .danger()
                                                .rounded(ButtonRounded::Large)
                                                .icon(icon("square"))
                                                .tooltip("Stop")
                                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                                        )
                                    } else {
                                        d.child(
                                            Button::new("send-ai")
                                                .small()
                                                .primary()
                                                .rounded(ButtonRounded::Large)
                                                .icon(icon("arrow-up"))
                                                .tooltip("Send (Enter)")
                                                .disabled(self.target.is_none())
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.send_input(window, cx)
                                                })),
                                        )
                                    }
                                }),
                        ),
                ),
            )
    }
}

/// Starters shown in an empty conversation.
const SUGGESTIONS: [&str; 3] = [
    "Give me a tour of this database",
    "Which tables reference each other?",
    "What are the largest tables, and what do they hold?",
];

fn agent_icon(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "sparkles",
        Agent::Copilot => "bot",
    }
}
