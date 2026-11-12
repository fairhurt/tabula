//! The connection form: new or edit, with a URL paste box, SSL mode and
//! what AI agents may do on this connection.

use gpui_kit::component::{
    StyledExt as _,
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, Window, div,
};

use super::icon;
use crate::db::Session;
use crate::profiles::{AiAccess, Profile, SslMode};
use crate::rt;

pub enum ConnectEvent {
    Saved { profile: Profile, password: Option<String>, connect: bool, renamed_from: Option<String> },
    Cancel,
}

pub struct ConnectForm {
    editing: Option<String>,
    url: Entity<InputState>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    database: Entity<InputState>,
    ssl: SslMode,
    ai: AiAccess,
    testing: bool,
    test_result: Option<Result<String, String>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ConnectEvent> for ConnectForm {}

impl ConnectForm {
    pub fn new(
        existing: Option<(Profile, Option<String>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (p, pw) = existing.clone().unwrap_or_default();
        let input = |v: &str, ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (v, ph) = (v.to_string(), ph.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
        };
        let url = input("", "Paste a URL: postgres://user:password@host:5432/database", window, cx);
        let name = input(&p.name, "My database", window, cx);
        let host = input(&p.host, "localhost", window, cx);
        let port = input(&p.port.to_string(), "5432", window, cx);
        let user = input(&p.user, "postgres", window, cx);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Password")
                .default_value(pw.clone().unwrap_or_default())
        });
        let database = input(&p.database, "postgres", window, cx);
        let sub = cx.subscribe_in(&url, window, |this, state, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::Change) {
                let v = state.read(cx).value().to_string();
                if let Ok((p, pw)) = Profile::from_url("", v.trim()) {
                    this.fill(&p, pw.as_deref(), window, cx);
                }
            }
        });
        ConnectForm {
            editing: existing.map(|(p, _)| p.name),
            url,
            name,
            host,
            port,
            user,
            password,
            database,
            ssl: p.ssl,
            ai: p.ai,
            testing: false,
            test_result: None,
            _subs: vec![sub],
        }
    }

    fn fill(&mut self, p: &Profile, pw: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let set = |e: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        };
        if self.name.read(cx).value().is_empty() {
            set(&self.name, p.name.clone(), window, cx);
        }
        set(&self.host, p.host.clone(), window, cx);
        set(&self.port, p.port.to_string(), window, cx);
        set(&self.user, p.user.clone(), window, cx);
        set(&self.database, p.database.clone(), window, cx);
        if let Some(pw) = pw {
            set(&self.password, pw.to_string(), window, cx);
        }
        self.ssl = p.ssl;
        cx.notify();
    }

    fn profile(&self, cx: &gpui_kit::App) -> Result<(Profile, Option<String>), String> {
        let v = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let port = v(&self.port).parse::<u16>().map_err(|_| "Port must be a number".to_string())?;
        let mut p = Profile {
            name: v(&self.name),
            host: v(&self.host),
            port,
            user: v(&self.user),
            database: v(&self.database),
            ssl: self.ssl,
            ai: self.ai,
        };
        if p.host.is_empty() {
            p.host = "localhost".into();
        }
        if p.name.is_empty() {
            p.name = format!("{}@{}", p.database, p.host);
        }
        let pw = self.password.read(cx).value().to_string();
        Ok((p, (!pw.is_empty()).then_some(pw)))
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let (p, pw) = match self.profile(cx) {
            Ok(x) => x,
            Err(e) => {
                self.test_result = Some(Err(e));
                cx.notify();
                return;
            }
        };
        self.testing = true;
        self.test_result = None;
        let fut = rt::spawn(async move { Ok(Session::connect(p, pw).await?.server_version.clone()) });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.testing = false;
                this.test_result = Some(match out {
                    Ok(v) => Ok(format!("Connected · PostgreSQL {v}")),
                    Err(e) => Err(e.to_string()),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save(&mut self, connect: bool, cx: &mut Context<Self>) {
        match self.profile(cx) {
            Ok((profile, password)) => cx.emit(ConnectEvent::Saved {
                renamed_from: self.editing.clone().filter(|n| *n != profile.name),
                profile,
                password,
                connect,
            }),
            Err(e) => {
                self.test_result = Some(Err(e));
                cx.notify();
            }
        }
    }

    fn field(label: &str, input: &Entity<InputState>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .flex_1()
            .child(div().text_xs().font_medium().child(label.to_string()))
            .child(Input::new(input).small())
    }
}

impl Render for ConnectForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ssl_buttons = SslMode::ALL.map(|m| {
            Button::new(m.label())
                .xsmall()
                .label(m.label())
                .when(self.ssl == m, |b| b.primary())
                .when(self.ssl != m, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ssl = m;
                    cx.notify();
                }))
        });
        let ai_buttons = AiAccess::ALL.map(|a| {
            Button::new(a.label())
                .xsmall()
                .label(a.label())
                .when(self.ai == a, |b| b.primary())
                .when(self.ai != a, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ai = a;
                    cx.notify();
                }))
        });
        let ai_help = match self.ai {
            AiAccess::Off => "Agents can't see this connection.",
            AiAccess::ReadOnly => "Agents can read the schema and run queries in a read-only transaction.",
            AiAccess::AskForWrites => "Read-only, and Tabula asks you before an agent's write runs.",
            AiAccess::ReadWrite => "Agents can run any statement. Use only for throwaway databases.",
        };
        v_flex()
            .w(super::rem(560.))
            .p_5()
            .gap_3()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .child(icon("database-plus").text_color(theme.primary))
                    .child(div().text_lg().font_semibold().child(if self.editing.is_some() {
                        "Edit connection"
                    } else {
                        "New connection"
                    })),
            )
            .child(Input::new(&self.url).small())
            .child(Self::field("Name", &self.name))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Self::field("Host", &self.host)))
                    .child(div().w(super::rem(90.)).child(Self::field("Port", &self.port))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(Self::field("User", &self.user))
                    .child(Self::field("Password", &self.password)),
            )
            .child(Self::field("Database", &self.database))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(super::rem(70.)).text_xs().font_medium().child("SSL"))
                    .children(ssl_buttons),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                h_flex()
                                    .w(super::rem(70.))
                                    .gap_1()
                                    .text_xs()
                                    .font_medium()
                                    .child(icon("sparkles").xsmall())
                                    .child("AI"),
                            )
                            .children(ai_buttons),
                    )
                    .child(
                        div()
                            .pl(super::rem(78.))
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(ai_help),
                    ),
            )
            .children(self.test_result.clone().map(|r| {
                let (text, color) = match r {
                    Ok(t) => (t, theme.success),
                    Err(e) => (e, theme.danger),
                };
                div()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(color.opacity(0.1))
                    .text_color(color)
                    .text_xs()
                    .child(text)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("test")
                            .small()
                            .outline()
                            .label("Test")
                            .disabled(self.testing)
                            .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                    )
                    .when(self.testing, |d| d.child(Spinner::new().small()))
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel")
                            .small()
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnectEvent::Cancel))),
                    )
                    .child(
                        Button::new("save")
                            .small()
                            .outline()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                    )
                    .child(
                        Button::new("save-connect")
                            .small()
                            .primary()
                            .label("Save & connect")
                            .on_click(cx.listener(|this, _, _, cx| this.save(true, cx))),
                    ),
            )
    }
}
