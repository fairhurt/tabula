//! The schema map: every table in a schema as a card, foreign keys as links,
//! laid out by a small physics simulation. Linked tables pull together, all
//! tables push apart, and dragging one drags its neighbours along.
//!
//! In the dark theme the map sits on a starfield; in the light theme on a
//! dotted grid.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenuItem},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, BorderStyle, Bounds, BoxShadow, ClickEvent, Context, Entity,
    EventEmitter, Hsla, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, PathBuilder, Pixels, Point, Render, ScrollEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div,
    fill, linear_color_stop, linear_gradient, point, px, quad, size, transparent_black,
};

use super::structure::kind_icon;
use super::{c, icon};
use crate::db::{DbObject, ForeignKey, MapTable, Session};
use crate::rt;

// Card geometry, in world units (pixels at zoom 1).
const CARD_W: f32 = 236.;
const HEADER_H: f32 = 34.;
const ROW_H: f32 = 21.;
const CARD_PAD: f32 = 6.;
/// Columns listed on a card before "+N more".
const MAX_COLUMNS: usize = 14;
/// Schemas with more tables than this open with the columns folded away.
const COMPACT_OVER: usize = 60;

const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 2.5;

// ------------------------------------------------------------------ physics

type V = (f32, f32);

/// The layout simulation: bodies are the cards' centres and half-sizes.
#[derive(Default)]
pub struct Sim {
    pub pos: Vec<V>,
    pub vel: Vec<V>,
    pub half: Vec<V>,
    /// Held in place (pinned, or being dragged); still pushes others.
    pub fixed: Vec<bool>,
    pub links: Vec<(usize, usize)>,
}

impl Sim {
    const REPEL: f32 = 110_000.;
    const SPRING: f32 = 0.018;
    const GRAVITY: f32 = 0.0035;
    const DAMPING: f32 = 0.82;
    const MARGIN: f32 = 56.;
    const MAX_SPEED: f32 = 42.;

    /// How far apart two linked cards want to be.
    fn rest_length(&self, a: usize, b: usize) -> f32 {
        self.half[a].0 + self.half[b].0 + 150.
    }

    /// Advance one step; returns the kinetic energy left, so the caller can
    /// stop stepping once the layout is at rest.
    pub fn step(&mut self) -> f32 {
        let n = self.pos.len();
        let mut force = vec![(0f32, 0f32); n];
        for i in 0..n {
            // A weak pull to the middle keeps unlinked tables from drifting off.
            force[i].0 -= self.pos[i].0 * Self::GRAVITY;
            force[i].1 -= self.pos[i].1 * Self::GRAVITY;
            for j in i + 1..n {
                let (mut dx, mut dy) = (self.pos[j].0 - self.pos[i].0, self.pos[j].1 - self.pos[i].1);
                if dx == 0. && dy == 0. {
                    // Exactly stacked: nudge apart, differently per pair.
                    dx = 0.5 + (i as f32 * 0.37).sin();
                    dy = 0.5 + (j as f32 * 0.61).cos();
                }
                let dist = (dx * dx + dy * dy).sqrt().max(1.);
                let (ux, uy) = (dx / dist, dy / dist);
                let mut f = Self::REPEL / (dist * dist);
                // Cards that overlap (or nearly) push apart much harder.
                let gap_x = dx.abs() - (self.half[i].0 + self.half[j].0);
                let gap_y = dy.abs() - (self.half[i].1 + self.half[j].1);
                if gap_x < Self::MARGIN && gap_y < Self::MARGIN {
                    f += (Self::MARGIN - gap_x.max(gap_y)) * 0.12;
                }
                force[i].0 -= ux * f;
                force[i].1 -= uy * f;
                force[j].0 += ux * f;
                force[j].1 += uy * f;
            }
        }
        for &(a, b) in &self.links {
            if a == b {
                continue;
            }
            let (dx, dy) = (self.pos[b].0 - self.pos[a].0, self.pos[b].1 - self.pos[a].1);
            let dist = (dx * dx + dy * dy).sqrt().max(1.);
            let f = (dist - self.rest_length(a, b)) * Self::SPRING;
            let (fx, fy) = (dx / dist * f, dy / dist * f);
            force[a].0 += fx;
            force[a].1 += fy;
            force[b].0 -= fx;
            force[b].1 -= fy;
        }
        let mut energy = 0.;
        for i in 0..n {
            if self.fixed[i] {
                self.vel[i] = (0., 0.);
                continue;
            }
            let (mut vx, mut vy) = (
                (self.vel[i].0 + force[i].0) * Self::DAMPING,
                (self.vel[i].1 + force[i].1) * Self::DAMPING,
            );
            let speed = (vx * vx + vy * vy).sqrt();
            if speed > Self::MAX_SPEED {
                vx *= Self::MAX_SPEED / speed;
                vy *= Self::MAX_SPEED / speed;
            }
            self.vel[i] = (vx, vy);
            self.pos[i].0 += vx;
            self.pos[i].1 += vy;
            energy += vx * vx + vy * vy;
        }
        energy
    }

    /// Moving slowly enough to stop animating.
    pub fn at_rest(&self, energy: f32) -> bool {
        energy < 0.004 * self.pos.len().max(1) as f32
    }

    /// Step until at rest, or `max_steps`.
    pub fn settle(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            let e = self.step();
            if self.at_rest(e) {
                break;
            }
        }
    }

    /// The box around every card, as (min, max).
    pub fn extent(&self) -> Option<(V, V)> {
        let mut it = self.pos.iter().zip(&self.half);
        let (p, h) = it.next()?;
        let mut min = (p.0 - h.0, p.1 - h.1);
        let mut max = (p.0 + h.0, p.1 + h.1);
        for (p, h) in it {
            min = (min.0.min(p.0 - h.0), min.1.min(p.1 - h.1));
            max = (max.0.max(p.0 + h.0), max.1.max(p.1 + h.1));
        }
        Some((min, max))
    }
}

/// A repeatable pseudo-random number in 0..1 for `n`.
fn noise(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x & 0x00FF_FFFF) as f32 / 0x0100_0000 as f32
}

// --------------------------------------------------------------------- view

pub enum ErdEvent {
    /// Open this table's rows, or its structure.
    Open { object: DbObject, structure: bool },
    AskAi(String),
}

enum Load {
    Loading,
    Failed(String),
    Ready,
}

struct Link {
    /// The table holding the foreign key, and the table it points at.
    from: usize,
    to: usize,
    from_cols: Vec<String>,
    to_cols: Vec<String>,
}

enum Drag {
    Pan { mouse: Point<Pixels>, cam: V },
    Body { ix: usize, grab: V, moved: bool, was_fixed: bool },
}

/// Colours for the map: space in the dark theme, paper in the light one.
#[derive(Clone)]
struct Palette {
    space: bool,
    bg_top: Hsla,
    bg_bottom: Hsla,
    card: Hsla,
    header: Hsla,
    border: Hsla,
    text: Hsla,
    muted: Hsla,
    accent: Hsla,
    key: Hsla,
    link: Hsla,
    glow: Hsla,
}

/// One foreign key, ready to paint: a curve in screen space.
struct Curve {
    a: V,
    b: V,
    ca: V,
    cb: V,
    /// 0 dimmed, 1 normal, 2 highlighted.
    level: u8,
    phase: f32,
}

pub struct ErdTab {
    pub title: SharedString,
    pub session: Arc<Session>,
    pub schema: String,
    load: Load,
    tables: Vec<MapTable>,
    /// For each table, its columns that are part of a foreign key.
    fk_cols: Vec<HashSet<String>>,
    links: Vec<Link>,
    /// Foreign keys to tables outside this schema (not drawn).
    outside: usize,
    sim: Sim,
    /// The world point at the top-left of the view, and the scale.
    cam: V,
    zoom: f32,
    /// Where the map was last painted, in window coordinates.
    view: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<Drag>,
    hover: Option<usize>,
    show_columns: bool,
    /// Twinkling stars and pulses on the links.
    motion: bool,
    /// The layout is still moving.
    awake: bool,
    fit_pending: bool,
    /// A camera position and zoom being eased towards.
    fly: Option<(V, f32)>,
    search: Entity<InputState>,
    found: Option<usize>,
    started: Instant,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ErdEvent> for ErdTab {}

impl ErdTab {
    pub fn new(session: Arc<Session>, schema: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Find a table…"));
        let sub = cx.subscribe(&search, |this, input, e: &InputEvent, cx| match e {
            InputEvent::Change => {
                let q = input.read(cx).value().trim().to_lowercase();
                this.found = (!q.is_empty())
                    .then(|| {
                        let names = this.tables.iter().map(|t| t.name.to_lowercase());
                        let exact = names.clone().position(|n| n.starts_with(&q));
                        exact.or_else(|| names.clone().position(|n| n.contains(&q)))
                    })
                    .flatten();
                if let Some(ix) = this.found {
                    this.fly_to(ix);
                }
                cx.notify();
            }
            _ => {}
        });
        let mut tab = ErdTab {
            title: format!("{schema} map").into(),
            session,
            schema,
            load: Load::Loading,
            tables: vec![],
            fk_cols: vec![],
            links: vec![],
            outside: 0,
            sim: Sim::default(),
            cam: (0., 0.),
            zoom: 1.,
            view: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            hover: None,
            show_columns: true,
            motion: true,
            awake: false,
            fit_pending: false,
            fly: None,
            search,
            found: None,
            started: Instant::now(),
            _subs: vec![sub],
        };
        tab.reload(cx);
        tab
    }

    /// Stop the animation (headless screenshots, where it would never settle).
    pub fn still(&mut self) {
        self.motion = false;
    }

    pub fn shows(&self, session: &Arc<Session>, schema: &str) -> bool {
        Arc::ptr_eq(&self.session, session) && self.schema == schema
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.load = Load::Loading;
        let (session, schema) = (self.session.clone(), self.schema.clone());
        let fut = rt::spawn(async move { session.schema_map(&schema).await });
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            let _ = this.update(cx, |this, cx| {
                match out {
                    Ok((tables, keys)) => this.set(tables, keys),
                    Err(e) => this.load = Load::Failed(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set(&mut self, tables: Vec<MapTable>, keys: Vec<ForeignKey>) {
        let index = |name: &str| tables.iter().position(|t| t.name == name);
        self.fk_cols = vec![HashSet::new(); tables.len()];
        self.links.clear();
        self.outside = 0;
        for k in keys {
            let Some(from) = index(&k.from_table) else { continue };
            self.fk_cols[from].extend(k.from_cols.iter().cloned());
            match index(&k.to_table).filter(|_| k.to_schema == self.schema) {
                Some(to) => self.links.push(Link { from, to, from_cols: k.from_cols, to_cols: k.to_cols }),
                None => self.outside += 1,
            }
        }
        self.show_columns = tables.len() <= COMPACT_OVER;
        self.tables = tables;
        self.load = Load::Ready;
        self.layout(false);
        self.fit_pending = true;
    }

    fn card_height(&self, ix: usize) -> f32 {
        if !self.show_columns {
            return HEADER_H;
        }
        let n = self.tables[ix].columns.len();
        let rows = n.min(MAX_COLUMNS) + usize::from(n > MAX_COLUMNS);
        HEADER_H + rows as f32 * ROW_H + CARD_PAD
    }

    fn resize_cards(&mut self) {
        self.sim.half = (0..self.tables.len()).map(|i| (CARD_W / 2., self.card_height(i) / 2.)).collect();
    }

    /// Lay the tables out from scratch. They start bunched in the middle and
    /// spread to where the simulation says they rest.
    fn layout(&mut self, scatter: bool) {
        let n = self.tables.len();
        let seed = if scatter { self.started.elapsed().subsec_nanos() } else { 0 };
        self.sim = Sim {
            // A sunflower spiral: an even, repeatable start.
            pos: (0..n)
                .map(|i| {
                    let r = 190. * ((i + 1) as f32).sqrt();
                    let a = i as f32 * 2.399_963 + noise(seed.wrapping_add(i as u32)) * if scatter { 6.28 } else { 0. };
                    (r * a.cos() * 1.5, r * a.sin())
                })
                .collect(),
            vel: vec![(0., 0.); n],
            half: vec![],
            fixed: vec![false; n],
            links: self.links.iter().map(|l| (l.from, l.to)).collect(),
        };
        self.resize_cards();
        self.sim.settle(900);
        // Pull everything towards the middle, then let it spring back out.
        for p in &mut self.sim.pos {
            *p = (p.0 * 0.35, p.1 * 0.35);
        }
        self.awake = true;
    }

    // ------------------------------------------------------------- camera

    fn view_size(&self) -> V {
        let b = self.view.get();
        (b.size.width.as_f32(), b.size.height.as_f32())
    }

    fn to_world(&self, at: Point<Pixels>) -> V {
        let o = self.view.get().origin;
        (
            self.cam.0 + (at.x - o.x).as_f32() / self.zoom,
            self.cam.1 + (at.y - o.y).as_f32() / self.zoom,
        )
    }

    /// The camera that shows every table. Uses where the tables will rest,
    /// not where they are mid-flight.
    fn fit_target(&self) -> Option<(V, f32)> {
        let (vw, vh) = self.view_size();
        if vw <= 0. || vh <= 0. {
            return None;
        }
        let mut rest = Sim {
            pos: self.sim.pos.clone(),
            vel: self.sim.vel.clone(),
            half: self.sim.half.clone(),
            fixed: self.sim.fixed.clone(),
            links: self.sim.links.clone(),
        };
        rest.settle(900);
        let (min, max) = rest.extent()?;
        let (w, h) = (max.0 - min.0 + 140., max.1 - min.1 + 200.);
        let zoom = (vw / w).min(vh / h).clamp(MIN_ZOOM, 1.15);
        let centre = ((min.0 + max.0) / 2., (min.1 + max.1) / 2. - 20. / zoom);
        Some(((centre.0 - vw / zoom / 2., centre.1 - vh / zoom / 2.), zoom))
    }

    fn fit(&mut self, animate: bool) {
        if let Some((cam, zoom)) = self.fit_target() {
            if animate {
                self.fly = Some((cam, zoom));
            } else {
                (self.cam, self.zoom) = (cam, zoom);
            }
        }
    }

    /// Ease the camera over to a table.
    fn fly_to(&mut self, ix: usize) {
        let (vw, vh) = self.view_size();
        let zoom = self.zoom.max(0.8);
        let p = self.sim.pos[ix];
        self.fly = Some(((p.0 - vw / zoom / 2., p.1 - vh / zoom / 2.), zoom));
    }

    fn zoom_at(&mut self, at: Point<Pixels>, factor: f32) {
        let before = self.to_world(at);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.to_world(at);
        self.cam = (self.cam.0 + before.0 - after.0, self.cam.1 + before.1 - after.1);
        self.fly = None;
    }

    // -------------------------------------------------------------- mouse

    fn on_card_down(&mut self, ix: usize, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let w = self.to_world(e.position);
        let p = self.sim.pos[ix];
        self.drag = Some(Drag::Body {
            ix,
            grab: (w.0 - p.0, w.1 - p.1),
            moved: false,
            was_fixed: self.sim.fixed[ix],
        });
        self.fly = None;
        cx.stop_propagation();
    }

    fn on_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if e.pressed_button != Some(MouseButton::Left) {
            if self.drag.is_some() {
                self.end_drag(cx);
            }
            return;
        }
        let w = self.to_world(e.position);
        match &mut self.drag {
            Some(Drag::Pan { mouse, cam }) => {
                self.cam = (
                    cam.0 - (e.position.x - mouse.x).as_f32() / self.zoom,
                    cam.1 - (e.position.y - mouse.y).as_f32() / self.zoom,
                );
                cx.notify();
            }
            Some(Drag::Body { ix, grab, moved, .. }) => {
                let to = (w.0 - grab.0, w.1 - grab.1);
                if *moved || (to.0 - self.sim.pos[*ix].0).abs() + (to.1 - self.sim.pos[*ix].1).abs() > 2. {
                    *moved = true;
                    self.sim.pos[*ix] = to;
                    // Held by the mouse: the others move around it.
                    self.sim.fixed[*ix] = true;
                    self.awake = true;
                    cx.notify();
                }
            }
            None => {}
        }
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(Drag::Body { ix, moved, was_fixed, .. }) = self.drag.take() {
            // A table that was dragged stays where it was put.
            self.sim.fixed[ix] = moved || was_fixed;
            self.awake = true;
        }
        cx.notify();
    }

    fn object(&self, ix: usize) -> DbObject {
        let t = &self.tables[ix];
        DbObject {
            schema: self.schema.clone(),
            name: t.name.clone(),
            kind: t.kind,
            rows_estimate: t.rows_estimate,
            comment: None,
            signature: None,
        }
    }

    // ------------------------------------------------------------ drawing

    fn palette(&self, cx: &gpui_kit::App) -> Palette {
        let t = cx.theme();
        if t.is_dark() {
            Palette {
                space: true,
                bg_top: c(0x050816),
                bg_bottom: c(0x0f1a3c),
                card: c(0x0f172a).opacity(0.95),
                header: c(0x2563eb).opacity(0.32),
                border: c(0x3b82f6).opacity(0.55),
                text: c(0xe2e8f0),
                muted: c(0x94a3b8),
                accent: c(0x7db4ff),
                key: c(0xfcd34d),
                link: c(0x60a5fa),
                glow: c(0x3b82f6),
            }
        } else {
            Palette {
                space: false,
                bg_top: c(0xf8fafc),
                bg_bottom: c(0xeef2ff),
                card: t.popover,
                header: t.primary.opacity(0.10),
                border: t.primary.opacity(0.35),
                text: t.foreground,
                muted: t.muted_foreground,
                accent: t.primary,
                key: c(0xd97706),
                link: t.primary,
                glow: t.primary,
            }
        }
    }

    /// Where a link meets a card: beside the named column if it is listed,
    /// else beside the header. World coordinates.
    fn anchor(&self, ix: usize, cols: &[String], right: bool) -> V {
        let (p, h) = (self.sim.pos[ix], self.sim.half[ix]);
        let row = cols
            .first()
            .and_then(|col| self.tables[ix].columns.iter().position(|k| &k.name == col))
            .filter(|i| self.show_columns && *i < MAX_COLUMNS);
        let y = match row {
            Some(i) => p.1 - h.1 + HEADER_H + (i as f32 + 0.5) * ROW_H,
            None => p.1 - h.1 + HEADER_H / 2.,
        };
        (if right { p.0 + h.0 } else { p.0 - h.0 }, y)
    }

    fn curves(&self) -> Vec<Curve> {
        let screen = |w: V| ((w.0 - self.cam.0) * self.zoom, (w.1 - self.cam.1) * self.zoom);
        self.links
            .iter()
            .enumerate()
            .filter(|(_, l)| l.from != l.to)
            .map(|(i, l)| {
                let leftward = self.sim.pos[l.from].0 > self.sim.pos[l.to].0;
                let a = self.anchor(l.from, &l.from_cols, !leftward);
                let b = self.anchor(l.to, &l.to_cols, leftward);
                let reach = ((b.0 - a.0).abs() * 0.5).max(60.);
                let dir = if leftward { -1. } else { 1. };
                let level = match self.hover {
                    Some(h) if h == l.from || h == l.to => 2,
                    Some(_) => 0,
                    None => 1,
                };
                Curve {
                    a: screen(a),
                    b: screen(b),
                    ca: screen((a.0 + reach * dir, a.1)),
                    cb: screen((b.0 - reach * dir, b.1)),
                    level,
                    phase: noise(i as u32 + 7),
                }
            })
            .collect()
    }

    fn related(&self, ix: usize) -> bool {
        match self.hover {
            None => true,
            Some(h) => h == ix || self.links.iter().any(|l| (l.from == h && l.to == ix) || (l.to == h && l.from == ix)),
        }
    }

    fn render_card(&self, ix: usize, pal: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let z = self.zoom;
        let t = &self.tables[ix];
        let (p, h) = (self.sim.pos[ix], self.sim.half[ix]);
        let (x, y) = ((p.0 - h.0 - self.cam.0) * z, (p.1 - h.1 - self.cam.1) * z);
        let fixed = self.sim.fixed[ix];
        let found = self.found == Some(ix);
        let hovered = self.hover == Some(ix);
        // Bigger tables glow more.
        let weight = ((t.rows_estimate.unwrap_or(0).max(0) as f32 + 1.).log10() / 6.).clamp(0., 1.);
        let glow = if hovered || found { 1. } else { weight };
        let mono = cx.theme().mono_font_family.clone();
        let weak = cx.entity().downgrade();
        let more = t.columns.len().saturating_sub(MAX_COLUMNS);

        let header = h_flex()
            .h(px(HEADER_H * z))
            .px(px(10. * z))
            .gap(px(7. * z))
            // Clipping doesn't follow rounded corners, so the header rounds
            // its own to sit inside the card's.
            .rounded_t(px(9. * z - 1.))
            .when(!self.show_columns, |d| d.rounded_b(px(9. * z - 1.)))
            .bg(pal.header)
            .child(icon(kind_icon(t.kind)).size(px(15. * z)).text_color(pal.accent))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .font_semibold()
                    .text_size(px(13.5 * z))
                    .child(t.name.clone()),
            )
            .children(t.rows_estimate.filter(|n| *n > 0).map(|n| {
                div().text_size(px(10.5 * z)).text_color(pal.muted).child(short_count(n))
            }))
            .when(fixed, |d| {
                d.child(
                    div()
                        .id(("erd-pin", ix))
                        .cursor_pointer()
                        .child(icon("pin").size(px(12. * z)).text_color(pal.key))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.sim.fixed[ix] = false;
                            this.awake = true;
                            cx.notify();
                        })),
                )
            });

        let columns = self.show_columns.then(|| {
            v_flex()
                .children(t.columns.iter().take(MAX_COLUMNS).map(|col| {
                    let fk = self.fk_cols[ix].contains(&col.name);
                    h_flex()
                        .h(px(ROW_H * z))
                        .px(px(10. * z))
                        .gap(px(6. * z))
                        .text_size(px(11.5 * z))
                        .child(div().w(px(12. * z)).flex_shrink_0().map(|d| {
                            if col.primary_key {
                                d.child(icon("key-round").size(px(11. * z)).text_color(pal.key))
                            } else if fk {
                                d.child(icon("link").size(px(11. * z)).text_color(pal.accent))
                            } else {
                                d
                            }
                        }))
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .when(col.primary_key, |d| d.font_semibold())
                                .child(col.name.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(mono.clone())
                                .text_size(px(10. * z))
                                .text_color(pal.muted)
                                .child(short_type(&col.data_type)),
                        )
                }))
                .when(more > 0, |d| {
                    d.child(
                        div()
                            .h(px(ROW_H * z))
                            .px(px(28. * z))
                            .text_size(px(10.5 * z))
                            .text_color(pal.muted)
                            .child(format!("+{more} more")),
                    )
                })
        });

        let object = self.object(ix);
        div()
            .id(("erd-card", ix))
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(CARD_W * z))
            .h(px(h.1 * 2. * z))
            .rounded(px(9. * z))
            .overflow_hidden()
            .border_1()
            .border_color(if hovered || found { pal.accent } else { pal.border })
            .bg(pal.card)
            .text_color(pal.text)
            .shadow(vec![BoxShadow {
                color: pal.glow.opacity(if pal.space { 0.18 + 0.42 * glow } else { 0.10 + 0.22 * glow }),
                offset: point(px(0.), px(0.)),
                blur_radius: px((12. + 30. * glow) * z),
                spread_radius: px(if hovered || found { 2. } else { 0. }),
                inset: false,
            }])
            .when(!self.related(ix), |d| d.opacity(0.35))
            .cursor_pointer()
            .child(header)
            .children(columns)
            .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                if this.drag.is_some() {
                    return;
                }
                if *over {
                    this.hover = Some(ix);
                } else if this.hover == Some(ix) {
                    this.hover = None;
                }
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, e: &MouseDownEvent, _, cx| this.on_card_down(ix, e, cx)))
            .on_click(cx.listener({
                let object = object.clone();
                move |_, e: &ClickEvent, _, cx| {
                    if e.click_count() >= 2 {
                        cx.emit(ErdEvent::Open { object: object.clone(), structure: false });
                    }
                }
            }))
            .context_menu(move |menu, _, _| {
                let item = |label: &str, ic: &str, f: Box<dyn Fn(&mut ErdTab, &mut Context<ErdTab>)>| {
                    let weak = weak.clone();
                    PopupMenuItem::new(label.to_string()).icon(icon(ic)).on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| f(this, cx));
                    })
                };
                let (o1, o2, name) = (object.clone(), object.clone(), object.qualified());
                menu.item(item("Open data", "table", Box::new(move |_, cx| {
                    cx.emit(ErdEvent::Open { object: o1.clone(), structure: false })
                })))
                .item(item("Structure", "table-properties", Box::new(move |_, cx| {
                    cx.emit(ErdEvent::Open { object: o2.clone(), structure: true })
                })))
                .item(item("Ask AI about it", "sparkles", Box::new(move |_, cx| {
                    cx.emit(ErdEvent::AskAi(format!(
                        "Describe {name}: what it holds, how it relates to other tables, and a few useful queries."
                    )))
                })))
                .separator()
                .item(item(if fixed { "Unpin" } else { "Pin here" }, "pin", Box::new(move |this, cx| {
                    this.sim.fixed[ix] = !this.sim.fixed[ix];
                    this.awake = true;
                    cx.notify();
                })))
            })
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let summary = format!(
            "{} · {} table{} · {} link{}{}",
            self.schema,
            self.tables.len(),
            if self.tables.len() == 1 { "" } else { "s" },
            self.links.len(),
            if self.links.len() == 1 { "" } else { "s" },
            if self.outside > 0 { format!(" · {} to other schemas", self.outside) } else { String::new() },
        );
        h_flex()
            .absolute()
            .top_3()
            .left_3()
            .p_1()
            .gap_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover.opacity(0.92))
            .shadow_md()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon("orbit").small().ml_2().text_color(theme.primary))
            .child(div().px_1().text_sm().font_medium().child(summary))
            .child(div().w(px(1.)).h(super::rem(18.)).mx_1().bg(theme.border))
            .child(
                Button::new("erd-fit")
                    .small()
                    .ghost()
                    .icon(icon("maximize"))
                    .tooltip("Fit everything in view")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.fit(true);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("erd-layout")
                    .small()
                    .ghost()
                    .icon(icon("shuffle"))
                    .tooltip("Lay out again (unpins everything)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.layout(true);
                        this.fit(true);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("erd-columns")
                    .small()
                    .ghost()
                    .selected(self.show_columns)
                    .icon(icon("rows-3"))
                    .tooltip("Show or hide columns")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_columns = !this.show_columns;
                        this.resize_cards();
                        this.awake = true;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("erd-motion")
                    .small()
                    .ghost()
                    .selected(self.motion)
                    .icon(icon("sparkles"))
                    .tooltip("Animation on or off")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.motion = !this.motion;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("erd-reload")
                    .small()
                    .ghost()
                    .icon(icon("refresh-cw"))
                    .tooltip("Read the schema again")
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
            .child(div().w(super::rem(170.)).child(Input::new(&self.search).small()))
    }
}

/// Paint the background and the links. Coordinates are relative to `bounds`.
fn paint_map(bounds: Bounds<Pixels>, pal: &Palette, curves: &[Curve], cam: V, zoom: f32, t: f32, motion: bool, window: &mut Window) {
    let o = bounds.origin;
    let (w, h) = (bounds.size.width.as_f32(), bounds.size.height.as_f32());
    let dot = |window: &mut Window, x: f32, y: f32, r: f32, color: Hsla| {
        window.paint_quad(quad(
            Bounds::new(point(o.x + px(x - r), o.y + px(y - r)), size(px(r * 2.), px(r * 2.))),
            px(r),
            color,
            px(0.),
            transparent_black(),
            BorderStyle::default(),
        ));
    };
    window.paint_quad(fill(
        bounds,
        linear_gradient(180., linear_color_stop(pal.bg_top, 0.), linear_color_stop(pal.bg_bottom, 1.)),
    ));

    if pal.space {
        // Three layers of stars; nearer layers slide faster as the map pans.
        const TILE: (f32, f32) = (1400., 900.);
        for (layer, (depth, count, big)) in [(0.04, 90, 0.9), (0.10, 60, 1.3), (0.22, 34, 1.9)].into_iter().enumerate() {
            let shift = (-cam.0 * zoom * depth, -cam.1 * zoom * depth);
            for i in 0..count {
                let n = (layer as u32) * 1000 + i;
                let (bx, by) = (noise(n * 3 + 1) * TILE.0, noise(n * 3 + 2) * TILE.1);
                let twinkle = if motion {
                    0.6 + 0.4 * (t * (0.6 + noise(n * 3 + 3) * 1.8) + noise(n + 500) * 6.28).sin()
                } else {
                    0.8
                };
                let tint = match n % 7 {
                    0 => c(0xbfdbfe),
                    1 => c(0xfde68a),
                    _ => c(0xffffff),
                };
                let r = big * (0.5 + noise(n + 900) * 0.8);
                let (x0, y0) = ((bx + shift.0).rem_euclid(TILE.0), (by + shift.1).rem_euclid(TILE.1));
                let mut x = x0;
                while x < w {
                    let mut y = y0;
                    while y < h {
                        dot(window, x, y, r, tint.opacity((0.25 + 0.6 * noise(n + 300)) * twinkle));
                        y += TILE.1;
                    }
                    x += TILE.0;
                }
            }
        }
    } else {
        // A dotted grid that moves and scales with the map.
        let mut step = 40. * zoom;
        while step < 22. {
            step *= 2.;
        }
        let (x0, y0) = ((-cam.0 * zoom).rem_euclid(step), (-cam.1 * zoom).rem_euclid(step));
        let mut x = x0;
        while x < w {
            let mut y = y0;
            while y < h {
                dot(window, x, y, 1.1, pal.muted.opacity(0.28));
                y += step;
            }
            x += step;
        }
    }

    let at = |p: V| point(o.x + px(p.0), o.y + px(p.1));
    for cv in curves {
        let (alpha, width) = match cv.level {
            0 => (0.14, 1.4),
            1 => (0.6, 1.8),
            _ => (1., 2.6),
        };
        let mut path = PathBuilder::stroke(px(width * zoom.clamp(0.6, 1.4)));
        path.move_to(at(cv.a));
        path.cubic_bezier_to(at(cv.b), at(cv.ca), at(cv.cb));
        if let Ok(path) = path.build() {
            window.paint_path(path, pal.link.opacity(alpha));
        }
        let r = 3.4 * zoom.clamp(0.6, 1.4);
        dot(window, cv.a.0, cv.a.1, r * 0.7, pal.link.opacity(alpha));
        dot(window, cv.b.0, cv.b.1, r, pal.key.opacity(alpha));
        if motion && cv.level > 0 {
            // A light running along the link, from the row to what it references.
            let u = (t * 0.28 + cv.phase).fract();
            let v = 1. - u;
            let bez = |a: f32, ca: f32, cb: f32, b: f32| v * v * v * a + 3. * v * v * u * ca + 3. * v * u * u * cb + u * u * u * b;
            let (x, y) = (bez(cv.a.0, cv.ca.0, cv.cb.0, cv.b.0), bez(cv.a.1, cv.ca.1, cv.cb.1, cv.b.1));
            let fade = (u * 6.).min(1.) * ((1. - u) * 6.).min(1.);
            dot(window, x, y, r * 2.4, pal.link.opacity(0.22 * fade));
            dot(window, x, y, r * 1.1, c(0xffffff).opacity(0.9 * fade));
        }
    }
}

fn short_count(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

/// A column type, short enough for a card.
fn short_type(t: &str) -> String {
    let t = t
        .replace("timestamp with time zone", "timestamptz")
        .replace("timestamp without time zone", "timestamp")
        .replace("time with time zone", "timetz")
        .replace("time without time zone", "time")
        .replace("character varying", "varchar")
        .replace("character", "char")
        .replace("double precision", "float8")
        .replace("integer", "int")
        .replace("boolean", "bool");
    if t.chars().count() > 18 {
        format!("{}…", t.chars().take(17).collect::<String>())
    } else {
        t
    }
}

impl Render for ErdTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let pal = self.palette(cx);
        let ready = matches!(self.load, Load::Ready);

        // Advance the layout and the camera, and keep the frames coming
        // while anything moves.
        if ready {
            if self.fit_pending && self.view_size().0 > 0. {
                self.fit(false);
                self.fit_pending = false;
            }
            if self.awake {
                let mut energy = 0.;
                for _ in 0..2 {
                    energy = self.sim.step();
                }
                if self.sim.at_rest(energy) && self.drag.is_none() {
                    self.awake = false;
                }
            }
            if let Some((cam, zoom)) = self.fly {
                let ease = |from: f32, to: f32| from + (to - from) * 0.2;
                self.cam = (ease(self.cam.0, cam.0), ease(self.cam.1, cam.1));
                self.zoom = ease(self.zoom, zoom);
                if (self.cam.0 - cam.0).abs() + (self.cam.1 - cam.1).abs() < 0.5 && (self.zoom - zoom).abs() < 0.002 {
                    (self.cam, self.zoom) = (cam, zoom);
                    self.fly = None;
                }
            }
            if self.awake || self.motion || self.fly.is_some() || self.fit_pending {
                window.request_animation_frame();
            }
        }

        let (cam, zoom, motion) = (self.cam, self.zoom, self.motion);
        let t = self.started.elapsed().as_secs_f32();
        let curves = if ready { self.curves() } else { vec![] };
        let view = self.view.clone();
        let paint_pal = pal.clone();
        let backdrop = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                view.set(bounds);
                paint_map(bounds, &paint_pal, &curves, cam, zoom, t, motion, window);
            },
        )
        .absolute()
        .size_full();

        // Only the cards in view are built.
        let (vw, vh) = self.view_size();
        let visible = |ix: usize| {
            let (p, h) = (self.sim.pos[ix], self.sim.half[ix]);
            let (x0, y0) = ((p.0 - h.0 - cam.0) * zoom, (p.1 - h.1 - cam.1) * zoom);
            let (x1, y1) = ((p.0 + h.0 - cam.0) * zoom, (p.1 + h.1 - cam.1) * zoom);
            vw <= 0. || (x1 > -60. && y1 > -60. && x0 < vw + 60. && y0 < vh + 60.)
        };
        let cards: Vec<AnyElement> = if ready {
            (0..self.tables.len()).filter(|ix| visible(*ix)).map(|ix| self.render_card(ix, &pal, cx)).collect()
        } else {
            vec![]
        };

        let note = |text: String, color: Hsla| {
            div().absolute().inset_0().flex().items_center().justify_center().text_color(color).child(text)
        };
        div()
            .id("erd")
            .relative()
            .size_full()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                this.drag = Some(Drag::Pan { mouse: e.position, cam: this.cam });
                this.fly = None;
                cx.notify();
            }))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.on_move(e, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_scroll(cx.listener(|this, e: &ScrollEvent, window, cx| {
                let dy = e.delta.pixel_delta(window.line_height()).y.as_f32();
                this.zoom_at(e.position, (dy * 0.0016).exp());
                cx.notify();
            }))
            .child(backdrop)
            .children(cards)
            .map(|d| match &self.load {
                Load::Loading => d.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .text_color(pal.muted)
                        .child(Spinner::new().small())
                        .child("Reading the schema…"),
                ),
                Load::Failed(e) => d.child(note(e.clone(), theme.danger)),
                Load::Ready if self.tables.is_empty() => {
                    d.child(note(format!("No tables in {} yet.", self.schema), pal.muted))
                }
                Load::Ready => d.child(
                    div()
                        .absolute()
                        .bottom_3()
                        .left_3()
                        .text_xs()
                        .text_color(pal.muted)
                        .child("Drag a table to move it · drag the background to pan · scroll to zoom · double-click a table for its rows"),
                ),
            })
            .child(self.render_toolbar(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sim(n: usize, links: &[(usize, usize)]) -> Sim {
        Sim {
            pos: (0..n).map(|i| (i as f32 * 10., (i as f32 * 7.).sin() * 10.)).collect(),
            vel: vec![(0., 0.); n],
            half: vec![(118., 90.); n],
            fixed: vec![false; n],
            links: links.to_vec(),
        }
    }

    fn overlap(s: &Sim, a: usize, b: usize) -> bool {
        (s.pos[a].0 - s.pos[b].0).abs() < s.half[a].0 + s.half[b].0
            && (s.pos[a].1 - s.pos[b].1).abs() < s.half[a].1 + s.half[b].1
    }

    #[test]
    fn layout_comes_to_rest_without_overlaps() {
        let mut s = sim(6, &[(0, 1), (1, 2), (3, 1), (4, 5)]);
        s.settle(3000);
        let e = s.step();
        assert!(s.at_rest(e), "still moving: {e}");
        for a in 0..6 {
            for b in a + 1..6 {
                assert!(!overlap(&s, a, b), "{a} and {b} overlap: {:?} {:?}", s.pos[a], s.pos[b]);
            }
        }
    }

    #[test]
    fn linked_tables_sit_closer_than_unlinked_ones() {
        let mut s = sim(3, &[(0, 1)]);
        s.settle(3000);
        let d = |a: usize, b: usize| ((s.pos[a].0 - s.pos[b].0).powi(2) + (s.pos[a].1 - s.pos[b].1).powi(2)).sqrt();
        assert!(d(0, 1) < d(0, 2) && d(0, 1) < d(1, 2), "{} {} {}", d(0, 1), d(0, 2), d(1, 2));
    }

    #[test]
    fn a_pinned_table_stays_put() {
        let mut s = sim(4, &[(0, 1), (0, 2)]);
        s.fixed[0] = true;
        s.pos[0] = (500., -300.);
        s.settle(2000);
        assert_eq!(s.pos[0], (500., -300.));
        assert!(!overlap(&s, 0, 1) && !overlap(&s, 0, 2));
    }

    #[test]
    fn types_are_shortened() {
        assert_eq!(short_type("timestamp with time zone"), "timestamptz");
        assert_eq!(short_type("character varying(40)"), "varchar(40)");
        assert_eq!(short_type("integer"), "int");
    }
}
