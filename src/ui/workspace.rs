//! The main window: the object tree, tabs, the Ask AI panel, the status bar,
//! and the live link that lets agents show results and ask for approval.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::{
    StyledExt as _,
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenuItem},
    resizable::{h_resizable, resizable_panel},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    UniformListScrollHandle, WeakEntity, Window, div, px, uniform_list,
};

use super::ai_panel::{AiPanel, AiPanelEvent};
use super::connect::{ConnectEvent, ConnectForm};
use super::data::DataTab;
use super::erd::{ErdEvent, ErdTab};
use super::history::{HistoryEvent, HistoryView};
use super::query::{QueryEvent, QueryTab};
use super::structure::{StructureTab, kind_icon};
use super::{CloseTab, NewQuery, Refresh, ToggleAi, ToggleHistory, ZoomIn, ZoomOut, ZoomReset, icon};
use crate::agents::{Agent, McpStatus};
use crate::db::{DbObject, ObjectKind, Session};
use crate::history::History;
use crate::link::{self, LinkReply, LinkRequest};
use crate::profiles::{self, AiAccess, Profile};
use crate::rt;
use crate::settings::{self, Settings};

#[derive(Clone, PartialEq)]
enum LoadState {
    Idle,
    Loading,
    Ready,
    Failed(String),
}

struct SchemaNode {
    name: String,
    expanded: bool,
    state: LoadState,
    objects: Vec<DbObject>,
    open_groups: HashSet<ObjectKind>,
}

struct DbNode {
    name: String,
    session: Option<Arc<Session>>,
    state: LoadState,
    expanded: bool,
    schemas: Vec<SchemaNode>,
}

struct Conn {
    profile: Profile,
    /// Not saved (opened with --url).
    transient: bool,
    state: LoadState,
    expanded: bool,
    dbs: Vec<DbNode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Node {
    Conn(usize),
    Db(usize, usize),
    Schema(usize, usize, usize),
    Group(usize, usize, usize, ObjectKind),
    Object(usize, usize, usize, usize),
}

struct Row {
    node: Node,
    depth: usize,
    label: String,
    icon: &'static str,
    expandable: bool,
    expanded: bool,
    loading: bool,
    muted: bool,
    detail: Option<String>,
}

enum Tab {
    Query(Entity<QueryTab>),
    Data(Entity<DataTab>),
    Structure(Entity<StructureTab>),
    Erd(Entity<ErdTab>),
}

impl Tab {
    fn title(&self, cx: &gpui_kit::App) -> SharedString {
        match self {
            Tab::Query(t) => t.read(cx).title.clone(),
            Tab::Data(t) => t.read(cx).title.clone(),
            Tab::Structure(t) => t.read(cx).title.clone(),
            Tab::Erd(t) => t.read(cx).title.clone(),
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Tab::Query(_) => "square-terminal",
            Tab::Data(_) => "table",
            Tab::Structure(_) => "table-properties",
            Tab::Erd(_) => "orbit",
        }
    }

    /// Structure tabs don't keep a session.
    fn session(&self, cx: &gpui_kit::App) -> Option<Arc<Session>> {
        match self {
            Tab::Query(t) => Some(t.read(cx).session.clone()),
            Tab::Data(t) => Some(t.read(cx).session.clone()),
            Tab::Structure(_) => None,
            Tab::Erd(t) => Some(t.read(cx).session.clone()),
        }
    }

    fn view(&self) -> AnyElement {
        match self {
            Tab::Query(t) => t.clone().into_any_element(),
            Tab::Data(t) => t.clone().into_any_element(),
            Tab::Structure(t) => t.clone().into_any_element(),
            Tab::Erd(t) => t.clone().into_any_element(),
        }
    }
}

struct Activity {
    at: String,
    agent: String,
    connection: String,
    tool: String,
    detail: String,
    ok: bool,
}

struct Approval {
    agent: String,
    connection: String,
    sql: String,
    reply: Option<futures::channel::oneshot::Sender<LinkReply>>,
}

enum Modal {
    Connect(Entity<ConnectForm>),
    Agents,
    Approve(Approval),
    History(Entity<HistoryView>),
    /// Closing would throw away row edits that haven't been saved.
    Discard(Discard),
}

/// What is being closed, and how many unsaved edits go with it.
#[derive(Clone, Copy)]
enum Discard {
    Tab { tab: usize, edits: usize },
    Quit { edits: usize },
}

/// Views the headless screenshots step through.
#[derive(Clone, Copy, Debug)]
pub enum ShotStep {
    Tree,
    Data,
    Structure,
    Query,
    Map,
    /// Opens a table to edit; `Edit` then stages the edits.
    EditOpen,
    Edit,
    History,
    Ai,
    Connect,
    Agents,
    Approve,
}

impl ShotStep {
    pub const ALL: [ShotStep; 12] = [
        ShotStep::Tree,
        ShotStep::Data,
        ShotStep::Structure,
        ShotStep::Query,
        ShotStep::Map,
        ShotStep::EditOpen,
        ShotStep::Edit,
        ShotStep::History,
        ShotStep::Ai,
        ShotStep::Connect,
        ShotStep::Agents,
        ShotStep::Approve,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ShotStep::Tree => "tree",
            ShotStep::Data => "data",
            ShotStep::Structure => "structure",
            ShotStep::Query => "query",
            ShotStep::Map => "map",
            ShotStep::EditOpen => "edit-open",
            ShotStep::Edit => "edit",
            ShotStep::History => "history",
            ShotStep::Ai => "ai",
            ShotStep::Connect => "connect",
            ShotStep::Agents => "agents",
            ShotStep::Approve => "approve",
        }
    }
}

pub struct Workspace {
    conns: Vec<Conn>,
    selected: Option<Node>,
    tree_scroll: UniformListScrollHandle,
    tabs: Vec<Tab>,
    active: usize,
    ai: Entity<AiPanel>,
    show_ai: bool,
    modal: Option<Modal>,
    status: SharedString,
    activity: Vec<Activity>,
    exe: std::path::PathBuf,
    focus: FocusHandle,
    query_count: usize,
    settings: Settings,
    history: History,
    /// A newer release, once the startup check has found one.
    update: Option<crate::update::Release>,
    /// The update is being downloaded.
    updating: bool,
    _subs: Vec<Subscription>,
}

impl Workspace {
    /// `url`: connect to this database. `connect`: or to this saved connection.
    pub fn new(
        url: Option<String>,
        connect: Option<String>,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ai = cx.new(|cx| AiPanel::new(window, cx));
        let sub = cx.subscribe_in(&ai, window, |this, _, e: &AiPanelEvent, window, cx| match e {
            AiPanelEvent::Insert(sql) => this.put_sql(sql, false, window, cx),
            AiPanelEvent::Run(sql) => this.put_sql(sql, true, window, cx),
            AiPanelEvent::Prefs => {
                let (agent, models) = this.ai.read(cx).prefs();
                this.settings.agent = Some(agent);
                this.settings.models = models;
                this.save_settings();
            }
        });
        let settings = if live { settings::load() } else { Settings::default() };
        ai.update(cx, |a, _| a.set_prefs(settings.agent.as_deref(), settings.models.clone()));
        let mut conns: Vec<Conn> = profiles::load()
            .into_iter()
            .map(|p| Conn {
                profile: p,
                transient: false,
                state: LoadState::Idle,
                expanded: false,
                dbs: vec![],
            })
            .collect();
        let mut auto = None;
        if let Some(u) = url {
            match Profile::from_url("", &u) {
                Ok((p, pw)) => {
                    let found = conns.iter().position(|c| c.profile.summary() == p.summary());
                    let ix = found.unwrap_or_else(|| {
                        conns.push(Conn {
                            profile: Profile {
                                ssl: if p.host == "localhost" || p.host == "127.0.0.1" {
                                    profiles::SslMode::Disable
                                } else {
                                    p.ssl
                                },
                                ..p.clone()
                            },
                            transient: true,
                            state: LoadState::Idle,
                            expanded: false,
                            dbs: vec![],
                        });
                        conns.len() - 1
                    });
                    auto = Some((ix, pw));
                }
                Err(e) => eprintln!("--url: {e}"),
            }
        }
        if let Some(name) = connect {
            match conns.iter().position(|c| c.profile.name == name) {
                Some(ix) => auto = Some((ix, None)),
                None => eprintln!("--connect: no saved connection named {name:?}"),
            }
        }
        let mut ws = Workspace {
            conns,
            selected: None,
            tree_scroll: UniformListScrollHandle::new(),
            tabs: vec![],
            active: 0,
            ai,
            show_ai: false,
            modal: None,
            status: "Ready".into(),
            activity: vec![],
            exe: std::env::current_exe().unwrap_or_default(),
            focus: cx.focus_handle(),
            query_count: 0,
            settings,
            history: if live { History::open() } else { History::in_memory() },
            update: None,
            updating: false,
            _subs: vec![sub],
        };
        if live {
            // Remember where the window was, for next time.
            let this = cx.entity().downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                let place = window.window_bounds();
                let b = place.get_bounds();
                let _ = this.update(cx, |ws, _| {
                    ws.settings.window = Some(settings::WindowPlace {
                        x: b.origin.x.as_f32(),
                        y: b.origin.y.as_f32(),
                        width: b.size.width.as_f32(),
                        height: b.size.height.as_f32(),
                        maximized: matches!(place, gpui_kit::WindowBounds::Maximized(_)),
                    });
                    ws.save_settings();
                });
                // Don't close over unsaved row edits without asking.
                this.update(cx, |ws, cx| ws.may_quit(cx)).unwrap_or(true)
            });
        }
        if live && ws.settings.check_for_updates && crate::update::enabled() {
            ws.check_for_update(cx);
        }
        if let Some((ix, pw)) = auto {
            ws.connect(ix, pw, cx);
        }
        if live {
            ws.listen(window, cx);
        }
        ws
    }

    // ------------------------------------------------------------ live link

    fn listen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut rx = match link::listen() {
            Ok(rx) => rx,
            Err(e) => {
                self.status = format!("AI link unavailable: {e}").into();
                return;
            }
        };
        cx.spawn_in(window, async move |this, cx| {
            while let Some((req, reply)) = rx.next().await {
                let ok = this
                    .update_in(cx, |this, window, cx| this.on_link(req, reply, window, cx))
                    .is_ok();
                if !ok {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_link(
        &mut self,
        req: LinkRequest,
        reply: futures::channel::oneshot::Sender<LinkReply>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match req {
            LinkRequest::Ping => {
                let _ = reply.send(LinkReply::yes());
            }
            LinkRequest::Activity { agent, connection, tool, detail, ok } => {
                self.status = format!("{agent} used {tool} on {connection}").into();
                self.activity.push(Activity {
                    at: chrono::Local::now().format("%H:%M:%S").to_string(),
                    agent,
                    connection,
                    tool,
                    detail,
                    ok,
                });
                if self.activity.len() > 300 {
                    self.activity.remove(0);
                }
                let _ = reply.send(LinkReply::yes());
            }
            LinkRequest::Approve { agent, connection, sql } => {
                if matches!(self.modal, Some(Modal::Approve(_))) {
                    let _ = reply.send(LinkReply::no("another approval is waiting"));
                } else {
                    self.modal = Some(Modal::Approve(Approval {
                        agent,
                        connection,
                        sql,
                        reply: Some(reply),
                    }));
                    window.activate_window();
                }
            }
            LinkRequest::Open { connection, sql, title } => {
                let Some(ci) = self.conns.iter().position(|c| c.profile.name == connection) else {
                    let _ = reply.send(LinkReply::no(format!("no connection named {connection}")));
                    return;
                };
                let title = title.unwrap_or_else(|| "AI query".into());
                self.open_query_on(ci, title, sql, true, window, cx);
                let _ = reply.send(LinkReply::yes());
            }
        }
        cx.notify();
    }

    fn decide(&mut self, allow: bool, cx: &mut Context<Self>) {
        if let Some(Modal::Approve(mut a)) = self.modal.take() {
            if let Some(r) = a.reply.take() {
                let _ = r.send(if allow { LinkReply::yes() } else { LinkReply::no("declined") });
            }
            self.status = format!(
                "{} {}'s statement on {}",
                if allow { "Allowed" } else { "Declined" },
                a.agent,
                a.connection
            )
            .into();
        }
        cx.notify();
    }

    // ------------------------------------------------------------ connections

    fn connect(&mut self, ci: usize, password: Option<String>, cx: &mut Context<Self>) {
        let Some(c) = self.conns.get_mut(ci) else { return };
        if c.state == LoadState::Loading {
            return;
        }
        c.state = LoadState::Loading;
        c.expanded = true;
        let p = c.profile.clone();
        let pw = password.or_else(|| profiles::password(&p.name));
        self.status = format!("Connecting to {}…", p.name).into();
        let fut = rt::spawn(async move {
            let s = Session::connect(p, pw).await?;
            let dbs = s.databases().await.unwrap_or_default();
            let schemas = s.schemas().await?;
            Ok((s, dbs, schemas))
        });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                let Some(c) = this.conns.get_mut(ci) else { return };
                match out {
                    Ok((s, mut dbs, schemas)) => {
                        c.state = LoadState::Ready;
                        if !dbs.contains(&c.profile.database) {
                            dbs.insert(0, c.profile.database.clone());
                        }
                        let version = s.server_version.clone();
                        c.dbs = dbs
                            .into_iter()
                            .map(|name| {
                                let current = name == c.profile.database;
                                DbNode {
                                    session: current.then(|| s.clone()),
                                    state: if current { LoadState::Ready } else { LoadState::Idle },
                                    expanded: current,
                                    schemas: if current { schema_nodes(&schemas) } else { vec![] },
                                    name,
                                }
                            })
                            .collect();
                        let di = c.dbs.iter().position(|d| d.session.is_some()).unwrap_or(0);
                        // Open the user schemas.
                        let user: Vec<usize> = (0..c.dbs[di].schemas.len())
                            .filter(|&si| !is_system(&c.dbs[di].schemas[si].name))
                            .collect();
                        let (name, db) = (c.profile.name.clone(), c.profile.database.clone());
                        this.status = format!("Connected to {name} · PostgreSQL {version}").into();
                        for si in user {
                            this.toggle(Node::Schema(ci, di, si), cx);
                        }
                        this.ai.update(cx, |a, cx| a.set_target(&name, &db, cx));
                    }
                    Err(e) => {
                        this.status = format!("Couldn't connect to {}", c.profile.name).into();
                        c.state = LoadState::Failed(e.to_string());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn disconnect(&mut self, ci: usize, cx: &mut Context<Self>) {
        if let Some(c) = self.conns.get_mut(ci) {
            c.state = LoadState::Idle;
            c.dbs.clear();
            c.expanded = false;
        }
        cx.notify();
    }

    fn open_db(&mut self, ci: usize, di: usize, cx: &mut Context<Self>) {
        let Some(base) = self.conns[ci].dbs.iter().find_map(|d| d.session.clone()) else {
            return;
        };
        let d = &mut self.conns[ci].dbs[di];
        if d.state == LoadState::Loading {
            return;
        }
        d.state = LoadState::Loading;
        d.expanded = true;
        let name = d.name.clone();
        let fut = rt::spawn(async move {
            let s = base.reconnect_to(&name).await?;
            let schemas = s.schemas().await?;
            Ok((s, schemas))
        });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                let Some(d) = this.conns.get_mut(ci).and_then(|c| c.dbs.get_mut(di)) else {
                    return;
                };
                match out {
                    Ok((s, schemas)) => {
                        d.session = Some(s);
                        d.schemas = schema_nodes(&schemas);
                        d.state = LoadState::Ready;
                    }
                    Err(e) => d.state = LoadState::Failed(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_schema(&mut self, ci: usize, di: usize, si: usize, cx: &mut Context<Self>) {
        let d = &mut self.conns[ci].dbs[di];
        let Some(s) = d.session.clone() else { return };
        let node = &mut d.schemas[si];
        node.state = LoadState::Loading;
        let name = node.name.clone();
        let fut = rt::spawn(async move { s.objects(&name).await });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                let Some(n) = this
                    .conns
                    .get_mut(ci)
                    .and_then(|c| c.dbs.get_mut(di))
                    .and_then(|d| d.schemas.get_mut(si))
                else {
                    return;
                };
                match out {
                    Ok(objs) => {
                        n.objects = objs;
                        n.state = LoadState::Ready;
                    }
                    Err(e) => n.state = LoadState::Failed(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        // Reload every expanded schema.
        let mut todo = vec![];
        for (ci, c) in self.conns.iter().enumerate() {
            for (di, d) in c.dbs.iter().enumerate() {
                for (si, s) in d.schemas.iter().enumerate() {
                    if s.expanded {
                        todo.push((ci, di, si));
                    }
                }
            }
        }
        for (ci, di, si) in todo {
            self.load_schema(ci, di, si, cx);
        }
        self.status = "Refreshed".into();
    }

    fn toggle(&mut self, node: Node, cx: &mut Context<Self>) {
        match node {
            Node::Conn(ci) => {
                let c = &mut self.conns[ci];
                match c.state {
                    LoadState::Idle | LoadState::Failed(_) => self.connect(ci, None, cx),
                    _ => c.expanded = !c.expanded,
                }
            }
            Node::Db(ci, di) => {
                let d = &mut self.conns[ci].dbs[di];
                if d.session.is_none() {
                    self.open_db(ci, di, cx);
                } else {
                    d.expanded = !d.expanded;
                }
            }
            Node::Schema(ci, di, si) => {
                let s = &mut self.conns[ci].dbs[di].schemas[si];
                s.expanded = !s.expanded;
                if s.expanded && s.state == LoadState::Idle {
                    if s.open_groups.is_empty() {
                        s.open_groups.insert(ObjectKind::Table);
                    }
                    self.load_schema(ci, di, si, cx);
                }
            }
            Node::Group(ci, di, si, k) => {
                let g = &mut self.conns[ci].dbs[di].schemas[si].open_groups;
                if !g.remove(&k) {
                    g.insert(k);
                }
            }
            Node::Object(..) => {}
        }
        cx.notify();
    }

    fn session_for(&self, node: Node) -> Option<Arc<Session>> {
        let (ci, di) = match node {
            Node::Conn(ci) => (ci, self.conns.get(ci)?.dbs.iter().position(|d| d.session.is_some())?),
            Node::Db(ci, di) | Node::Schema(ci, di, _) | Node::Group(ci, di, _, _) | Node::Object(ci, di, _, _) => (ci, di),
        };
        self.conns.get(ci)?.dbs.get(di)?.session.clone()
    }

    fn object(&self, node: Node) -> Option<DbObject> {
        let Node::Object(ci, di, si, oi) = node else { return None };
        self.conns.get(ci)?.dbs.get(di)?.schemas.get(si)?.objects.get(oi).cloned()
    }

    // ------------------------------------------------------------ tabs

    fn add_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        match &tab {
            Tab::Query(t) => {
                let s = cx.subscribe(t, Self::on_tab_event);
                self._subs.push(s);
            }
            Tab::Data(t) => {
                let s = cx.subscribe(t, Self::on_tab_event);
                self._subs.push(s);
            }
            Tab::Structure(t) => {
                let s = cx.subscribe(t, Self::on_tab_event);
                self._subs.push(s);
            }
            // `open_map` subscribes to these itself.
            Tab::Erd(_) => {}
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.sync_ai_target(cx);
        cx.notify();
    }

    fn on_tab_event<E>(&mut self, _: Entity<E>, e: &QueryEvent, cx: &mut Context<Self>) {
        match e {
            QueryEvent::Status(s) => self.status = s.clone().into(),
            QueryEvent::Ran(entry) => {
                if let Err(e) = self.history.record(entry.clone()) {
                    self.status = format!("Couldn't save query history: {e}").into();
                }
            }
            QueryEvent::AskAi(prompt) => self.ask_ai(prompt.clone(), cx),
            QueryEvent::FocusAi => {
                self.show_ai = true;
                self.sync_ai_target(cx);
                // Focus needs the window; do it on the next frame.
                let ai = self.ai.clone();
                cx.defer(move |cx| {
                    if let Some(w) = cx.active_window() {
                        let _ = w.update(cx, |_, window, cx| {
                            ai.update(cx, |a, cx| a.focus_input(window, cx))
                        });
                    }
                });
            }
        }
        cx.notify();
    }

    fn ask_ai(&mut self, prompt: String, cx: &mut Context<Self>) {
        self.show_ai = true;
        self.sync_ai_target(cx);
        self.ai.update(cx, |a, cx| a.ask(prompt, cx));
        cx.notify();
    }

    fn sync_ai_target(&mut self, cx: &mut Context<Self>) {
        let s = self
            .tabs
            .get(self.active)
            .and_then(|t| t.session(cx))
            .or_else(|| self.selected.and_then(|n| self.session_for(n)))
            .or_else(|| self.conns.iter().flat_map(|c| &c.dbs).find_map(|d| d.session.clone()));
        if let Some(s) = s {
            let (n, d) = (s.profile.name.clone(), s.profile.database.clone());
            self.ai.update(cx, |a, cx| a.set_target(&n, &d, cx));
        }
    }

    fn open_query_on(
        &mut self,
        ci: usize,
        title: String,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.conns[ci].dbs.iter().find_map(|d| d.session.clone());
        match session {
            Some(s) => self.open_query(s, title, sql, run, window, cx),
            None => {
                // Connect first, then open.
                self.connect(ci, None, cx);
                cx.spawn_in(window, async move |this, cx| {
                    for _ in 0..200 {
                        cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                        let done = this
                            .update_in(cx, |this, window, cx| {
                                let c = &this.conns[ci];
                                match (&c.state, c.dbs.iter().find_map(|d| d.session.clone())) {
                                    (LoadState::Ready, Some(s)) => {
                                        this.open_query(s, title.clone(), sql.clone(), run, window, cx);
                                        true
                                    }
                                    (LoadState::Failed(_), _) => true,
                                    _ => false,
                                }
                            })
                            .unwrap_or(true);
                        if done {
                            break;
                        }
                    }
                })
                .detach();
            }
        }
    }

    fn open_query(
        &mut self,
        session: Arc<Session>,
        title: String,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = cx.new(|cx| QueryTab::new(title, session, &sql, window, cx));
        if run {
            tab.update(cx, |t, cx| t.run(cx));
        }
        self.add_tab(Tab::Query(tab.clone()), cx);
        let focus = tab.read(cx).editor_focus(cx);
        window.focus(&focus, cx);
    }

    fn new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self
            .tabs
            .get(self.active)
            .and_then(|t| t.session(cx))
            .or_else(|| self.selected.and_then(|n| self.session_for(n)))
            .or_else(|| self.conns.iter().flat_map(|c| &c.dbs).find_map(|d| d.session.clone()));
        let Some(s) = session else {
            self.status = "Connect to a database first".into();
            cx.notify();
            return;
        };
        self.query_count += 1;
        let title = format!("Query {}", self.query_count);
        self.open_query(s, title, String::new(), false, window, cx);
    }

    fn open_object(&mut self, node: Node, structure: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(o), Some(s)) = (self.object(node), self.session_for(node)) else { return };
        self.open_table(s, o, structure, window, cx);
    }

    fn open_table(&mut self, s: Arc<Session>, o: DbObject, structure: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Reuse an open tab for the same object.
        let existing = self.tabs.iter().position(|t| match t {
            Tab::Data(d) if !structure => d.read(cx).object == o,
            Tab::Structure(d) if structure => d.read(cx).object == o,
            _ => false,
        });
        if let Some(i) = existing {
            self.active = i;
            cx.notify();
            return;
        }
        if structure || !o.kind.has_data() {
            let t = cx.new(|cx| StructureTab::new(s, o, window, cx));
            self.add_tab(Tab::Structure(t), cx);
        } else {
            let t = cx.new(|cx| DataTab::new(s, o, window, cx));
            self.add_tab(Tab::Data(t), cx);
        }
    }

    /// The schema a node belongs to, or failing that the first schema of
    /// the user's own that is connected.
    fn schema_for(&self, node: Option<Node>) -> Option<(Arc<Session>, String)> {
        let first_of = |ci: usize, di: usize| -> Option<(Arc<Session>, String)> {
            let d = self.conns.get(ci)?.dbs.get(di)?;
            let s = d.schemas.iter().find(|s| !is_system(&s.name) && !s.objects.is_empty())
                .or_else(|| d.schemas.iter().find(|s| !is_system(&s.name)))?;
            Some((d.session.clone()?, s.name.clone()))
        };
        let from_node = node.and_then(|n| match n {
            Node::Schema(ci, di, si) | Node::Group(ci, di, si, _) | Node::Object(ci, di, si, _) => {
                let d = self.conns.get(ci)?.dbs.get(di)?;
                Some((d.session.clone()?, d.schemas.get(si)?.name.clone()))
            }
            Node::Db(ci, di) => first_of(ci, di),
            Node::Conn(ci) => (0..self.conns.get(ci)?.dbs.len()).find_map(|di| first_of(ci, di)),
        });
        from_node.or_else(|| {
            (0..self.conns.len())
                .flat_map(|ci| (0..self.conns[ci].dbs.len()).map(move |di| (ci, di)))
                .find_map(|(ci, di)| first_of(ci, di))
        })
    }

    /// Open the schema map for a node's schema.
    fn open_map(&mut self, node: Option<Node>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((session, schema)) = self.schema_for(node) else {
            self.status = "Connect to a database first".into();
            cx.notify();
            return;
        };
        if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::Erd(e) if e.read(cx).shows(&session, &schema))) {
            self.active = i;
            cx.notify();
            return;
        }
        let tab = cx.new(|cx| ErdTab::new(session, schema, window, cx));
        let sub = cx.subscribe_in(&tab, window, |this, tab, e: &ErdEvent, window, cx| match e {
            ErdEvent::Open { object, structure } => {
                let session = tab.read(cx).session.clone();
                this.open_table(session, object.clone(), *structure, window, cx);
            }
            ErdEvent::AskAi(prompt) => this.ask_ai(prompt.clone(), cx),
        });
        self._subs.push(sub);
        self.add_tab(Tab::Erd(tab), cx);
    }

    /// Row edits staged in a tab and not saved.
    fn unsaved_in(&self, ix: usize, cx: &gpui_kit::App) -> usize {
        match self.tabs.get(ix) {
            Some(Tab::Data(t)) => t.read(cx).pending(cx),
            _ => 0,
        }
    }

    /// Close a tab, asking first if that would lose unsaved row edits.
    fn ask_to_close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let edits = self.unsaved_in(ix, cx);
        if edits == 0 {
            self.close_tab(ix, cx);
        } else if self.modal.is_none() {
            self.active = ix;
            self.modal = Some(Modal::Discard(Discard::Tab { tab: ix, edits }));
            cx.notify();
        }
    }

    /// Whether the window may close now. If tabs hold unsaved row edits,
    /// asks instead, and the answer closes it.
    fn may_quit(&mut self, cx: &mut Context<Self>) -> bool {
        let edits: usize = (0..self.tabs.len()).map(|i| self.unsaved_in(i, cx)).sum();
        if edits == 0 {
            return true;
        }
        // Another dialog (an agent's approval, say) keeps its place.
        if self.modal.is_none() || matches!(self.modal, Some(Modal::Agents | Modal::History(_))) {
            self.modal = Some(Modal::Discard(Discard::Quit { edits }));
        }
        cx.notify();
        false
    }

    fn render_discard(&self, what: Discard, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (edits, title, confirm) = match what {
            Discard::Tab { tab, edits } => {
                let name = self.tabs.get(tab).map(|t| t.title(cx).to_string()).unwrap_or_default();
                (edits, format!("Close {name} without saving?"), "Discard and close")
            }
            Discard::Quit { edits } => (edits, "Quit without saving?".to_string(), "Discard and quit"),
        };
        v_flex()
            .w(super::rem(460.))
            .p_5()
            .gap_3()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.warning.opacity(0.6))
            .rounded(theme.radius_lg)
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .child(icon("pencil-line").text_color(theme.warning))
                    .child(div().text_lg().font_semibold().child(title)),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                "{edits} row change{} {} not been saved to the database and will be lost.",
                if edits == 1 { "" } else { "s" },
                if edits == 1 { "has" } else { "have" },
            )))
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(div().flex_1())
                    .child(
                        Button::new("discard-keep")
                            .small()
                            .outline()
                            .label("Keep editing")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.modal = None;
                                cx.notify();
                            })),
                    )
                    .child(Button::new("discard-confirm").small().danger().label(confirm).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.modal = None;
                            match what {
                                Discard::Tab { tab, .. } => this.close_tab(tab, cx),
                                // Past the question: quit for real.
                                Discard::Quit { .. } => cx.quit(),
                            }
                        },
                    ))),
            )
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.tabs.remove(ix);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.sync_ai_target(cx);
            cx.notify();
        }
    }

    /// SQL from the AI panel: into the active query tab, or a new one.
    fn put_sql(&mut self, sql: &str, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Tab::Query(t)) = self.tabs.get(self.active) {
            let t = t.clone();
            t.update(cx, |t, cx| {
                if run {
                    t.set_sql(sql, window, cx);
                    t.run(cx);
                } else {
                    t.insert_sql(sql, window, cx);
                }
            });
            return;
        }
        let session = self
            .tabs
            .get(self.active)
            .and_then(|t| match t {
                Tab::Data(d) => Some(d.read(cx).session.clone()),
                _ => None,
            })
            .or_else(|| self.conns.iter().flat_map(|c| &c.dbs).find_map(|d| d.session.clone()));
        if let Some(s) = session {
            self.query_count += 1;
            let title = format!("Query {}", self.query_count);
            self.open_query(s, title, sql.to_string(), run, window, cx);
        }
    }

    // ------------------------------------------------------------ history

    fn toggle_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::History(_))) {
            self.modal = None;
            cx.notify();
            return;
        }
        if self.modal.is_some() {
            return;
        }
        let view = cx.new(|cx| HistoryView::new(self.history.entries(), window, cx));
        let sub = cx.subscribe_in(&view, window, |this, _, e: &HistoryEvent, window, cx| {
            match e {
                HistoryEvent::Insert(sql) => {
                    this.modal = None;
                    this.put_sql(sql, false, window, cx);
                }
                HistoryEvent::Run(sql) => {
                    this.modal = None;
                    this.put_sql(sql, true, window, cx);
                }
                HistoryEvent::Clear => {
                    this.status = match this.history.clear() {
                        Ok(()) => "Query history cleared".into(),
                        Err(e) => format!("Couldn't clear query history: {e}").into(),
                    };
                }
                HistoryEvent::Close => this.modal = None,
            }
            cx.notify();
        });
        self._subs.push(sub);
        view.update(cx, |v, cx| v.focus(window, cx));
        self.modal = Some(Modal::History(view));
        cx.notify();
    }

    // ------------------------------------------------------------ updates

    /// Ask GitHub, quietly, whether a newer release is out.
    fn check_for_update(&mut self, cx: &mut Context<Self>) {
        let fut = rt::spawn(async { tokio::task::spawn_blocking(crate::update::check).await? });
        cx.spawn(async move |this, cx| {
            // Offline, or no release yet: nothing to say.
            if let Ok(Some(release)) = fut.await {
                let _ = this.update(cx, |this, cx| {
                    this.status = format!("Tabula {} is available", release.version).into();
                    this.update = Some(release);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Install the newer release: on Windows download its installer, check
    /// it and run it; elsewhere open its page.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(release) = self.update.clone() else { return };
        if self.updating {
            return;
        }
        let installer = if cfg!(windows) { release.installer.clone() } else { None };
        let Some(installer) = installer else {
            cx.open_url(&release.page);
            return;
        };
        self.updating = true;
        self.status = format!("Downloading Tabula {}…", release.version).into();
        let fut = rt::spawn(async move { tokio::task::spawn_blocking(move || crate::update::download(&installer)).await? });
        cx.spawn(async move |this, cx| {
            let downloaded = fut.await;
            let _ = this.update(cx, |this, cx| {
                this.updating = false;
                match downloaded.and_then(|path| start_installer(&path)) {
                    // The installer closes this window and starts the new version.
                    Ok(()) => cx.quit(),
                    Err(e) => {
                        this.status = format!("Couldn't update: {e}").into();
                        cx.open_url(&release.page);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------------ settings

    fn save_settings(&mut self) {
        if let Err(e) = settings::save(&self.settings) {
            self.status = format!("Couldn't save settings: {e}").into();
        }
    }

    /// Switch between the dark and light themes, and remember the choice.
    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let dark = !cx.theme().is_dark();
        self.settings.theme = Some(if dark { "dark" } else { "light" }.into());
        self.save_settings();
        super::set_theme(cx, dark, self.settings.font_size());
        self.status = if dark { "Dark theme" } else { "Light theme" }.into();
        cx.notify();
    }

    /// Change the text size by `delta` pixels, or reset it with `None`.
    fn zoom(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        let size = match delta {
            Some(d) => (self.settings.font_size() + d).clamp(settings::MIN_FONT_SIZE, settings::MAX_FONT_SIZE),
            None => settings::DEFAULT_FONT_SIZE,
        };
        self.settings.font_size = size;
        self.save_settings();
        super::set_font_size(cx, size);
        self.status = format!("Text size {size:.0}px").into();
        cx.notify();
    }

    // ------------------------------------------------------------ modals

    fn open_connect(&mut self, edit: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = edit.and_then(|ci| {
            let p = self.conns.get(ci)?.profile.clone();
            let pw = profiles::password(&p.name);
            Some((p, pw))
        });
        let form = cx.new(|cx| ConnectForm::new(existing, window, cx));
        let sub = cx.subscribe(&form, |this, _, e: &ConnectEvent, cx| match e {
            ConnectEvent::Cancel => {
                this.modal = None;
                cx.notify();
            }
            ConnectEvent::Saved { profile, password, connect, renamed_from } => {
                this.save_profile(profile.clone(), password.clone(), renamed_from.clone(), *connect, cx);
            }
        });
        self._subs.push(sub);
        self.modal = Some(Modal::Connect(form));
        cx.notify();
    }

    fn save_profile(
        &mut self,
        p: Profile,
        password: Option<String>,
        renamed_from: Option<String>,
        connect: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(old) = &renamed_from {
            let _ = profiles::remove(old);
        }
        if let Err(e) = profiles::upsert(p.clone()).and_then(|_| profiles::set_password(&p.name, password.as_deref())) {
            self.status = format!("Couldn't save: {e}").into();
            cx.notify();
            return;
        }
        let key = renamed_from.unwrap_or_else(|| p.name.clone());
        let ci = match self.conns.iter().position(|c| c.profile.name == key) {
            Some(ci) => {
                self.conns[ci].profile = p.clone();
                self.conns[ci].transient = false;
                ci
            }
            None => {
                self.conns.push(Conn {
                    profile: p.clone(),
                    transient: false,
                    state: LoadState::Idle,
                    expanded: false,
                    dbs: vec![],
                });
                self.conns.len() - 1
            }
        };
        self.modal = None;
        self.status = format!("Saved {}", p.name).into();
        if connect {
            self.disconnect(ci, cx);
            self.connect(ci, password, cx);
        }
        cx.notify();
    }

    fn delete_profile(&mut self, ci: usize, cx: &mut Context<Self>) {
        let c = self.conns.remove(ci);
        if !c.transient {
            let _ = profiles::remove(&c.profile.name);
        }
        self.selected = None;
        self.status = format!("Removed {}", c.profile.name).into();
        cx.notify();
    }

    // ------------------------------------------------------------ screenshots

    pub fn shot(&mut self, step: ShotStep, window: &mut Window, cx: &mut Context<Self>) {
        let find = |this: &Self, name: &str| -> Option<Node> {
            for (ci, c) in this.conns.iter().enumerate() {
                for (di, d) in c.dbs.iter().enumerate() {
                    for (si, s) in d.schemas.iter().enumerate() {
                        if let Some(oi) = s.objects.iter().position(|o| o.name == name) {
                            return Some(Node::Object(ci, di, si, oi));
                        }
                    }
                }
            }
            None
        };
        match step {
            ShotStep::Tree => {
                // Expand views too.
                for c in &mut self.conns {
                    for d in &mut c.dbs {
                        for s in &mut d.schemas {
                            if s.expanded {
                                s.open_groups.insert(ObjectKind::View);
                                s.open_groups.insert(ObjectKind::Function);
                            }
                        }
                    }
                }
                self.selected = find(self, "orders");
            }
            ShotStep::Data => {
                if let Some(n) = find(self, "orders") {
                    self.open_object(n, false, window, cx);
                }
            }
            ShotStep::Structure => {
                if let Some(n) = find(self, "orders") {
                    self.open_object(n, true, window, cx);
                }
            }
            ShotStep::Query => {
                if let Some(s) = self.conns.iter().flat_map(|c| &c.dbs).find_map(|d| d.session.clone()) {
                    self.query_count += 1;
                    self.open_query(
                        s,
                        "Query 1".into(),
                        "-- Revenue by status, last 30 days\nSELECT o.status, count(*) AS orders, sum(p.price * i.quantity) AS revenue\nFROM shop.orders o\nJOIN shop.order_items i ON i.order_id = o.id\nJOIN shop.products p ON p.id = i.product_id\nWHERE o.placed_at > now() - interval '30 days'\nGROUP BY o.status\nORDER BY revenue DESC;".into(),
                        true,
                        window,
                        cx,
                    );
                }
            }
            ShotStep::Map => {
                let node = find(self, "orders");
                self.open_map(node, window, cx);
                if let Some(Tab::Erd(t)) = self.tabs.last() {
                    t.update(cx, |t, _| t.still());
                }
            }
            ShotStep::EditOpen => {
                if let Some(n) = find(self, "products") {
                    self.open_object(n, false, window, cx);
                }
            }
            ShotStep::Edit => {
                if let Some(Tab::Data(t)) = self.tabs.get(self.active) {
                    t.clone().update(cx, |t, cx| t.shot_edits(cx));
                }
            }
            ShotStep::History => {
                let entry = |at: &str, sql: &str, ms: u64, rows: Option<u64>, error: Option<&str>| crate::history::Entry {
                    at: format!("{}T{at}:00+00:00", chrono::Local::now().format("%Y-%m-%d")),
                    connection: "demo".into(),
                    database: "demo".into(),
                    sql: sql.into(),
                    elapsed_ms: ms,
                    rows,
                    error: error.map(str::to_string),
                };
                for e in [
                    entry("09:12", "SELECT count(*) FROM shop.customers WHERE country = 'DE';", 4, Some(1), None),
                    entry("09:15", "SELECT c.name, count(*) AS orders\nFROM shop.orders o\nJOIN shop.customers c ON c.id = o.customer_id\nGROUP BY c.id\nORDER BY orders DESC\nLIMIT 20;", 38, Some(20), None),
                    entry("09:21", "SELECT * FROM shop.order WHERE status = 'paid';", 2, None, Some("relation \"shop.order\" does not exist")),
                    entry("09:22", "SELECT * FROM shop.orders WHERE status = 'paid' ORDER BY placed_at DESC LIMIT 100;", 11, Some(100), None),
                    entry("09:40", "UPDATE shop.products SET price = price * 1.05 WHERE 'sale' = ANY(tags);", 7, Some(41), None),
                    entry("10:02", "-- Revenue by status, last 30 days\nSELECT o.status, count(*) AS orders, sum(p.price * i.quantity) AS revenue\nFROM shop.orders o\nJOIN shop.order_items i ON i.order_id = o.id\nJOIN shop.products p ON p.id = i.product_id\nWHERE o.placed_at > now() - interval '30 days'\nGROUP BY o.status\nORDER BY revenue DESC;", 64, Some(4), None),
                ] {
                    let _ = self.history.record(e);
                }
                self.toggle_history(window, cx);
            }
            ShotStep::Ai => {
                self.modal = None;
                self.show_ai = true;
                self.sync_ai_target(cx);
                self.ai.update(cx, |a, cx| a.shot_conversation(cx));
            }
            ShotStep::Connect => {
                self.show_ai = false;
                self.open_connect(None, window, cx);
            }
            ShotStep::Agents => {
                self.activity = vec![
                    Activity { at: "14:02:11".into(), agent: "claude-code".into(), connection: "demo".into(), tool: "describe_table".into(), detail: "shop.orders".into(), ok: true },
                    Activity { at: "14:02:13".into(), agent: "claude-code".into(), connection: "demo".into(), tool: "run_query".into(), detail: "select status, count(*) from shop.orders group by 1".into(), ok: true },
                    Activity { at: "14:03:40".into(), agent: "copilot".into(), connection: "demo".into(), tool: "run_query".into(), detail: "delete from shop.order_items".into(), ok: false },
                ];
                self.modal = Some(Modal::Agents);
            }
            ShotStep::Approve => {
                self.modal = Some(Modal::Approve(Approval {
                    agent: "claude-code".into(),
                    connection: "demo".into(),
                    sql: "UPDATE shop.orders SET status = 'cancelled'\nWHERE status = 'pending' AND placed_at < now() - interval '30 days';".into(),
                    reply: None,
                }));
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        for (ci, c) in self.conns.iter().enumerate() {
            let (loading, detail) = match &c.state {
                LoadState::Loading => (true, None),
                LoadState::Failed(_) => (false, Some("failed".to_string())),
                _ => (false, None),
            };
            rows.push(Row {
                node: Node::Conn(ci),
                depth: 0,
                label: c.profile.name.clone(),
                icon: if c.state == LoadState::Ready { "server" } else { "server-off" },
                expandable: true,
                expanded: c.expanded,
                loading,
                muted: c.state != LoadState::Ready,
                detail: detail.or_else(|| (c.profile.ai != AiAccess::Off).then(|| "AI".to_string())),
            });
            if !c.expanded {
                continue;
            }
            for (di, d) in c.dbs.iter().enumerate() {
                rows.push(Row {
                    node: Node::Db(ci, di),
                    depth: 1,
                    label: d.name.clone(),
                    icon: "database",
                    expandable: true,
                    expanded: d.expanded,
                    loading: d.state == LoadState::Loading,
                    muted: d.session.is_none(),
                    detail: None,
                });
                if !d.expanded {
                    continue;
                }
                for (si, s) in d.schemas.iter().enumerate() {
                    rows.push(Row {
                        node: Node::Schema(ci, di, si),
                        depth: 2,
                        label: s.name.clone(),
                        icon: if s.expanded { "folder-open" } else { "folder" },
                        expandable: true,
                        expanded: s.expanded,
                        loading: s.state == LoadState::Loading,
                        muted: is_system(&s.name),
                        detail: None,
                    });
                    if !s.expanded || s.state != LoadState::Ready {
                        continue;
                    }
                    for kind in ObjectKind::ORDER {
                        let items: Vec<(usize, &DbObject)> =
                            s.objects.iter().enumerate().filter(|(_, o)| o.kind == kind).collect();
                        if items.is_empty() {
                            continue;
                        }
                        let open = s.open_groups.contains(&kind);
                        rows.push(Row {
                            node: Node::Group(ci, di, si, kind),
                            depth: 3,
                            label: kind.label().to_string(),
                            icon: kind_icon(kind),
                            expandable: true,
                            expanded: open,
                            loading: false,
                            muted: true,
                            detail: Some(items.len().to_string()),
                        });
                        if !open {
                            continue;
                        }
                        for (oi, o) in items {
                            rows.push(Row {
                                node: Node::Object(ci, di, si, oi),
                                depth: 4,
                                label: match &o.signature {
                                    Some(sig) => format!("{}({sig})", o.name),
                                    None => o.name.clone(),
                                },
                                icon: kind_icon(kind),
                                expandable: false,
                                expanded: false,
                                loading: false,
                                muted: false,
                                detail: o.rows_estimate.filter(|n| *n > 0).map(short_count),
                            });
                        }
                    }
                }
            }
        }
        rows
    }

    fn on_row_click(&mut self, node: Node, e: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(node);
        match node {
            Node::Object(..) => {
                if e.click_count() >= 2 {
                    self.open_object(node, false, window, cx);
                }
            }
            n => self.toggle(n, cx),
        }
        self.sync_ai_target(cx);
        cx.notify();
    }

    fn row_menu(
        this: WeakEntity<Self>,
        node: Node,
        menu: gpui_kit::component::menu::PopupMenu,
        conn_ready: bool,
        has_data: bool,
        qualified: Option<String>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let act = |label: &str, ic: &str, f: Box<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>| {
            let this = this.clone();
            PopupMenuItem::new(label.to_string())
                .icon(icon(ic))
                .on_click(move |_, window, cx| {
                    let _ = this.update(cx, |w, cx| f(w, window, cx));
                })
        };
        match node {
            Node::Conn(ci) => {
                let mut m = menu;
                if conn_ready {
                    m = m
                        .item(act("New query", "square-terminal", Box::new(move |w, window, cx| {
                            w.selected = Some(Node::Conn(ci));
                            w.new_query(window, cx)
                        })))
                        .item(act("Disconnect", "unplug", Box::new(move |w, _, cx| w.disconnect(ci, cx))));
                } else {
                    m = m.item(act("Connect", "plug", Box::new(move |w, _, cx| w.connect(ci, None, cx))));
                }
                m.separator()
                    .item(act("Edit…", "square-pen", Box::new(move |w, window, cx| w.open_connect(Some(ci), window, cx))))
                    .item(act("Remove", "trash-2", Box::new(move |w, _, cx| w.delete_profile(ci, cx))))
            }
            Node::Object(..) => {
                let q = qualified.unwrap_or_default();
                let q2 = q.clone();
                let mut m = menu;
                if has_data {
                    m = m.item(act("Open data", "table", Box::new(move |w, window, cx| w.open_object(node, false, window, cx))));
                }
                m.item(act("Structure", "table-properties", Box::new(move |w, window, cx| w.open_object(node, true, window, cx))))
                    .when(has_data, |m| {
                        m.item(act("New query (SELECT)", "square-terminal", Box::new(move |w, window, cx| {
                            if let Some(s) = w.session_for(node) {
                                w.query_count += 1;
                                let title = format!("Query {}", w.query_count);
                                w.open_query(s, title, format!("SELECT *\nFROM {q}\nLIMIT 100;"), false, window, cx);
                            }
                        })))
                    })
                    .item(act("Ask AI about it", "sparkles", Box::new({
                        let q = q2.clone();
                        move |w, _, cx| {
                            w.selected = Some(node);
                            w.ask_ai(format!("Describe {q}: what it holds, how it relates to other tables, and a few useful queries."), cx);
                        }
                    })))
                    .separator()
                    .item(act("Copy name", "copy", Box::new(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(q2.clone()));
                    })))
            }
            Node::Schema(..) | Node::Db(..) | Node::Group(..) => menu
                .item(act("Schema map", "orbit", Box::new(move |w, window, cx| w.open_map(Some(node), window, cx))))
                .item(act("New query", "square-terminal", Box::new(move |w, window, cx| {
                    w.selected = Some(node);
                    w.new_query(window, cx)
                })))
                .item(act("Refresh", "refresh-cw", Box::new(|w, _, cx| w.refresh(cx)))),
        }
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = self.rows();
        let count = rows.len();
        let list = uniform_list(
            "object-tree",
            count,
            cx.processor(move |ws, range: std::ops::Range<usize>, _window, cx| {
                let rows = ws.rows();
                let theme = cx.theme().clone();
                range
                    .filter_map(|i| rows.get(i).map(|r| (i, r)))
                    .map(|(i, r)| {
                        let node = r.node;
                        let selected = ws.selected == Some(node);
                        let conn_ready = match node {
                            Node::Conn(ci) => ws.conns[ci].state == LoadState::Ready,
                            _ => false,
                        };
                        let obj = ws.object(node);
                        let has_data = obj.as_ref().is_some_and(|o| o.kind.has_data());
                        let qualified = obj.as_ref().map(|o| o.qualified());
                        let weak = cx.entity().downgrade();
                        h_flex()
                            .id(("tree-row", i))
                            .w_full()
                            .h(super::rem(24.))
                            .pl(super::rem(6. + r.depth as f32 * 14.))
                            .pr_2()
                            .gap_1()
                            .text_sm()
                            .cursor_pointer()
                            .when(selected, |d| d.bg(theme.sidebar_accent))
                            .hover(|d| d.bg(theme.sidebar_accent.opacity(0.6)))
                            .child(
                                div().w(super::rem(12.)).flex_shrink_0().when(r.expandable, |d| {
                                    d.child(
                                        icon(if r.expanded { "chevron-down" } else { "chevron-right" })
                                            .xsmall()
                                            .text_color(theme.muted_foreground),
                                    )
                                }),
                            )
                            .child(if r.loading {
                                Spinner::new().xsmall().into_any_element()
                            } else {
                                icon(r.icon)
                                    .small()
                                    .text_color(if r.muted { theme.muted_foreground } else { theme.primary })
                                    .into_any_element()
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .when(r.muted, |d| d.text_color(theme.muted_foreground))
                                    .child(r.label.clone()),
                            )
                            .children(r.detail.clone().map(|d| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(d)
                            }))
                            .on_click(cx.listener(move |ws, e: &ClickEvent, window, cx| {
                                ws.on_row_click(node, e, window, cx)
                            }))
                            .context_menu(move |menu, _, _| {
                                Workspace::row_menu(weak.clone(), node, menu, conn_ready, has_data, qualified.clone())
                            })
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.tree_scroll)
        .size_full();
                let failed = self.conns.iter().find_map(|c| match &c.state {
            LoadState::Failed(e) => Some((c.profile.name.clone(), e.clone())),
            _ => None,
        });
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().flex_1().pl_1().text_xs().font_semibold().text_color(theme.muted_foreground).child("CONNECTIONS"))
                    .child(
                        Button::new("open-map")
                            .xsmall()
                            .ghost()
                            .icon(icon("orbit"))
                            .tooltip("Schema map: tables and how they link")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let node = this.selected;
                                this.open_map(node, window, cx)
                            })),
                    )
                    .child(
                        Button::new("add-conn")
                            .xsmall()
                            .ghost()
                            .icon(icon("plus"))
                            .tooltip("New connection")
                            .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, window, cx))),
                    )
                    .child(
                        Button::new("refresh-tree")
                            .xsmall()
                            .ghost()
                            .icon(icon("refresh-cw"))
                            .tooltip("Refresh (Ctrl+R)")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().py_1().child(if count == 0 {
                v_flex()
                    .p_4()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No connections yet")
                    .child(
                        Button::new("first-conn")
                            .small()
                            .primary()
                            .icon(icon("database-plus"))
                            .label("Add a connection")
                            .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, window, cx))),
                    )
                    .into_any_element()
            } else {
                list.into_any_element()
            }))
            .children(failed.map(|(name, e)| {
                div()
                    .m_2()
                    .p_2()
                    .rounded(theme.radius)
                    .bg(theme.danger.opacity(0.1))
                    .text_color(theme.danger)
                    .text_xs()
                    .child(format!("{name}: {e}"))
            }))
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .id("tab-strip")
            .h(super::rem(34.))
            .flex_shrink_0()
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .overflow_x_scroll()
            .children(self.tabs.iter().enumerate().map(|(i, t)| {
                let active = i == self.active;
                let running = matches!(t, Tab::Query(q) if q.read(cx).is_running());
                h_flex()
                    .id(("tab", i))
                    .h_full()
                    .px_3()
                    .gap_2()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(theme.border)
                    .text_sm()
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme.tab_active).text_color(theme.tab_active_foreground))
                    .when(!active, |d| d.text_color(theme.tab_foreground).hover(|d| d.bg(theme.tab_active.opacity(0.5))))
                    .child(if running {
                        Spinner::new().xsmall().into_any_element()
                    } else {
                        icon(t.icon()).xsmall().text_color(if active { theme.primary } else { theme.muted_foreground }).into_any_element()
                    })
                    .child(t.title(cx))
                    .child(
                        Button::new(("close-tab", i))
                            .xsmall()
                            .ghost()
                            .icon(icon("x"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.ask_to_close_tab(i, cx)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active = i;
                        this.sync_ai_target(cx);
                        cx.notify();
                    }))
            }))
            .child(
                Button::new("new-tab")
                    .xsmall()
                    .ghost()
                    .mx_1()
                    .icon(icon("plus"))
                    .tooltip("New query (Ctrl+T)")
                    .on_click(cx.listener(|this, _, window, cx| this.new_query(window, cx))),
            )
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tip = |k: &str, what: &str| {
            h_flex()
                .gap_3()
                .child(
                    div()
                        .w(super::rem(110.))
                        .text_right()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .text_color(theme.primary)
                        .child(k.to_string()),
                )
                .child(div().text_sm().text_color(theme.muted_foreground).child(what.to_string()))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .child(gpui_kit::img(super::logo()).size(super::rem(84.)))
            .child(div().text_xl().font_semibold().child("Tabula"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Pick a table on the left, or start a query."),
            )
            .child(
                v_flex()
                    .gap_1()
                    .pt_2()
                    .child(tip("Double-click", "a table to browse its rows"))
                    .child(tip("Ctrl+T", "new query tab"))
                    .child(tip("Ctrl+Enter", "run the query"))
                    .child(tip("Ctrl+H", "query history"))
                    .child(tip("Ctrl+Shift+A", "Ask AI panel"))
                    .child(tip("Ctrl+= / Ctrl+-", "larger or smaller text"))
                    .child(tip("Right-click", "more actions")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("welcome-new")
                            .small()
                            .outline()
                            .icon(icon("database-plus"))
                            .label("New connection")
                            .on_click(cx.listener(|this, _, window, cx| this.open_connect(None, window, cx))),
                    )
                    .child(
                        Button::new("welcome-map")
                            .small()
                            .outline()
                            .icon(icon("orbit"))
                            .label("Schema map")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let node = this.selected;
                                this.open_map(node, window, cx)
                            })),
                    )
                    .child(
                        Button::new("welcome-agents")
                            .small()
                            .outline()
                            .icon(icon("bot"))
                            .label("Connect AI agents")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.modal = Some(Modal::Agents);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let exe = self.exe.clone();
        v_flex()
            .w(super::rem(640.))
            .max_h(super::rem(620.))
            .p_5()
            .gap_3()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .child(icon("bot").text_color(theme.primary))
                    .child(div().text_lg().font_semibold().flex_1().child("AI agents"))
                    .child(
                        Button::new("close-agents")
                            .xsmall()
                            .ghost()
                            .icon(icon("x"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.modal = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Connect Tabula to your agents and they can explore your databases and run queries from their own sessions, on your own subscription. Each connection decides what agents may do (read-only by default)."),
            )
            .children(Agent::ALL.map(|a| {
                let status = a.mcp_status(&exe);
                let installed = a.installed();
                let (badge, color) = match status {
                    McpStatus::Current => ("Connected", theme.success),
                    McpStatus::Outdated => ("Points at another Tabula", theme.warning),
                    McpStatus::Missing => ("Not connected", theme.muted_foreground),
                };
                let exe = exe.clone();
                h_flex()
                    .p_3()
                    .gap_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(icon(if a == Agent::Claude { "sparkles" } else { "bot" }).text_color(theme.primary))
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(div().font_semibold().text_sm().child(a.label()))
                            .child(
                                div().text_xs().text_color(theme.muted_foreground).child(if installed {
                                    a.mcp_config().map(|p| p.display().to_string()).unwrap_or_default()
                                } else {
                                    format!("Not installed. {}", a.install_hint())
                                }),
                            ),
                    )
                    .child(div().text_xs().text_color(color).child(badge))
                    .child(if status == McpStatus::Current {
                        Button::new(SharedString::from(format!("disc-{a:?}")))
                            .small()
                            .outline()
                            .label("Disconnect")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.status = match a.disconnect() {
                                    Ok(_) => format!("Removed Tabula from {}", a.label()),
                                    Err(e) => e.to_string(),
                                }
                                .into();
                                cx.notify();
                            }))
                    } else {
                        Button::new(SharedString::from(format!("conn-{a:?}")))
                            .small()
                            .primary()
                            .label("Connect")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.status = match a.connect(&exe) {
                                    Ok(p) => format!("Added Tabula to {} ({})", a.label(), p.display()),
                                    Err(e) => e.to_string(),
                                }
                                .into();
                                cx.notify();
                            }))
                    })
            }))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Or by hand: claude mcp add tabula -- <path to tabula> mcp"),
            )
            .child(div().pt_2().text_sm().font_semibold().child("Recent agent activity"))
            .child(
                v_flex()
                    .id("activity")
                    .max_h(super::rem(200.))
                    .overflow_y_scroll()
                    .gap_1()
                    .when(self.activity.is_empty(), |d| {
                        d.child(div().text_xs().text_color(theme.muted_foreground).child("Nothing yet. Activity shows here while Tabula is open."))
                    })
                    .children(self.activity.iter().rev().take(100).map(|a| {
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(div().text_color(theme.muted_foreground).child(a.at.clone()))
                            .child(icon(if a.ok { "check" } else { "x" }).xsmall().text_color(if a.ok { theme.success } else { theme.danger }))
                            .child(div().font_medium().child(format!("{} · {}", a.agent, a.tool)))
                            .child(div().text_color(theme.muted_foreground).child(a.connection.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .font_family(theme.mono_font_family.clone())
                                    .child(a.detail.lines().next().unwrap_or("").to_string()),
                            )
                    })),
            )
    }

    fn render_approval(&self, a: &Approval, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .w(super::rem(600.))
            .p_5()
            .gap_3()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.warning.opacity(0.6))
            .rounded(theme.radius_lg)
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .child(icon("shield-alert").text_color(theme.warning))
                    .child(div().text_lg().font_semibold().child("An agent wants to change data")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} asks to run this on {}:", a.agent, a.connection)),
            )
            .child(
                div()
                    .id("approve-sql")
                    .max_h(super::rem(260.))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_size(super::rem(12.))
                    .child(a.sql.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child("It runs once, exactly as shown."))
                    .child(
                        Button::new("deny")
                            .small()
                            .outline()
                            .label("Don't allow")
                            .on_click(cx.listener(|this, _, _, cx| this.decide(false, cx))),
                    )
                    .child(
                        Button::new("allow")
                            .small()
                            .warning()
                            .label("Allow once")
                            .on_click(cx.listener(|this, _, _, cx| this.decide(true, cx))),
                    ),
            )
    }

    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let connected = self
            .conns
            .iter()
            .flat_map(|c| &c.dbs)
            .filter(|d| d.session.is_some())
            .count();
        h_flex()
            .h(super::rem(26.))
            .px_3()
            .gap_3()
            .flex_shrink_0()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(div().flex_1().overflow_hidden().text_ellipsis().whitespace_nowrap().child(self.status.clone()))
            .child(format!("{connected} open"))
            .children(self.update.as_ref().map(|release| {
                Button::new("status-update")
                    .xsmall()
                    .primary()
                    .icon(icon("download"))
                    .label(format!("Update to {}", release.version))
                    .tooltip(if cfg!(windows) && release.installer.is_some() {
                        "Download the new version, install it and restart Tabula"
                    } else {
                        "Open the download page"
                    })
                    .loading(self.updating)
                    .on_click(cx.listener(|this, _, _, cx| this.install_update(cx)))
            }))
            .child(
                Button::new("status-history")
                    .xsmall()
                    .ghost()
                    .icon(icon("history"))
                    .label("History")
                    .tooltip("Query history (Ctrl+H)")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_history(window, cx))),
            )
            .child(
                Button::new("status-theme")
                    .xsmall()
                    .ghost()
                    .icon(icon(if cx.theme().is_dark() { "sun" } else { "moon" }))
                    .tooltip(if cx.theme().is_dark() { "Switch to the light theme" } else { "Switch to the dark theme" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_theme(cx))),
            )
            .child(
                Button::new("status-zoom")
                    .xsmall()
                    .ghost()
                    .icon(icon("a-large-small"))
                    .label(format!("{:.0}px", self.settings.font_size()))
                    .tooltip("Text size: Ctrl+= / Ctrl+- to change, click to reset")
                    .on_click(cx.listener(|this, _, _, cx| this.zoom(None, cx))),
            )
            .child(
                Button::new("status-agents")
                    .xsmall()
                    .ghost()
                    .icon(icon("bot"))
                    .label(if self.activity.is_empty() {
                        "Agents".to_string()
                    } else {
                        format!("Agents · {}", self.activity.len())
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.modal = Some(Modal::Agents);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("status-ai")
                    .xsmall()
                    .when(self.show_ai, |b| b.primary())
                    .when(!self.show_ai, |b| b.ghost())
                    .icon(icon("sparkles"))
                    .label("Ask AI")
                    .tooltip("Ctrl+Shift+A")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_ai = !this.show_ai;
                        this.sync_ai_target(cx);
                        cx.notify();
                    })),
            )
    }
}

#[cfg(windows)]
fn start_installer(path: &std::path::Path) -> anyhow::Result<()> {
    crate::update::run_installer(path)
}

#[cfg(not(windows))]
fn start_installer(_: &std::path::Path) -> anyhow::Result<()> {
    anyhow::bail!("updates install themselves on Windows only")
}

fn schema_nodes(names: &[String]) -> Vec<SchemaNode> {
    names
        .iter()
        .map(|n| SchemaNode {
            name: n.clone(),
            expanded: false,
            state: LoadState::Idle,
            objects: vec![],
            open_groups: HashSet::new(),
        })
        .collect()
}

fn is_system(schema: &str) -> bool {
    schema == "pg_catalog" || schema == "information_schema"
}

fn short_count(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let main = match self.tabs.get(self.active) {
            Some(t) => t.view(),
            None => self.render_welcome(cx).into_any_element(),
        };
        let center = v_flex()
            .size_full()
            .when(!self.tabs.is_empty(), |d| d.child(self.render_tabs(cx)))
            .child(div().flex_1().min_h_0().child(main));
        let mut split = h_resizable("main-split")
            .child(
                resizable_panel()
                    .size(px(270.))
                    .size_range(px(180.)..px(600.))
                    .child(self.render_tree(cx)),
            )
            .child(resizable_panel().child(center));
        if self.show_ai {
            split = split.child(
                resizable_panel()
                    .size(px(380.))
                    .size_range(px(280.)..px(800.))
                    .child(self.ai.clone()),
            );
        }
        let modal = self.modal.as_ref().map(|m| match m {
            Modal::Connect(f) => f.clone().into_any_element(),
            Modal::Agents => self.render_agents(cx).into_any_element(),
            Modal::Approve(a) => self.render_approval(a, cx).into_any_element(),
            Modal::History(h) => h.clone().into_any_element(),
            Modal::Discard(what) => self.render_discard(*what, cx).into_any_element(),
        });
        let closable = matches!(self.modal, Some(Modal::Agents | Modal::History(_)));
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &NewQuery, window, cx| this.new_query(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                let i = this.active;
                this.ask_to_close_tab(i, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleAi, _, cx| {
                this.show_ai = !this.show_ai;
                this.sync_ai_target(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &ToggleHistory, window, cx| this.toggle_history(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(-1.), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .child(div().flex_1().min_h_0().child(split))
            .child(self.render_status(cx))
            .children(modal.map(|m| {
                div()
                    .id("modal-scrim")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui_kit::black().opacity(0.45))
                    .when(closable, |d| {
                        d.on_click(cx.listener(|this, _, _, cx| {
                            this.modal = None;
                            cx.notify();
                        }))
                    })
                    .child(
                        div()
                            .id("modal-body")
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(m),
                    )
            }))
    }
}
