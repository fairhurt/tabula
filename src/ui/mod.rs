//! The Tabula window.

mod ai_panel;
mod connect;
mod data;
mod erd;
mod grid;
mod history;
mod query;
mod structure;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::{Icon, Theme, ThemeMode};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    App, AppContext as _, Bounds, HeadlessAppContext, Hsla, KeyBinding, Pixels, Rems,
    SharedString, TitlebarOptions, WindowBounds, WindowOptions, point, px, rems, rgb, size,
};

use crate::settings;

pub use workspace::Workspace;

gpui_kit::actions!(
    tabula,
    [
        RunQuery,
        NewQuery,
        CloseTab,
        ToggleAi,
        AskAiInEditor,
        Refresh,
        NextPage,
        PrevPage,
        CopySelection,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ToggleHistory,
        SaveChanges,
        EditCell,
    ]
);

/// A Lucide icon by name, e.g. `icon("database")`.
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

pub fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// The app icon (rendered from `assets/icon.svg` by build.rs).
pub fn logo() -> Arc<gpui_kit::Image> {
    static LOGO: std::sync::OnceLock<Arc<gpui_kit::Image>> = std::sync::OnceLock::new();
    LOGO.get_or_init(|| {
        Arc::new(gpui_kit::Image::from_bytes(
            gpui_kit::ImageFormat::Png,
            include_bytes!(concat!(env!("OUT_DIR"), "/icon.png")).to_vec(),
        ))
    })
    .clone()
}

/// A length that follows the text size: `v` is its pixel size at 13px text.
pub fn rem(v: f32) -> Rems {
    rems(v / 13.)
}

/// Like [`rem`], for the places that need pixels.
pub fn scale(cx: &App, v: f32) -> Pixels {
    use gpui_kit::component::ActiveTheme as _;
    px(v * cx.theme().font_size.as_f32() / 13.)
}

/// Set the UI text size; rem-based sizes and the SQL editor follow it.
pub fn set_font_size(cx: &mut App, size: f32) {
    Theme::update(cx, |t| {
        t.font_size = px(size);
        t.mono_font_size = px(size - 1.);
    });
    cx.refresh_windows();
}

/// A key with Ctrl on Linux and Windows, Cmd on macOS.
macro_rules! key {
    ($k:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $k)
        } else {
            concat!("ctrl-", $k)
        }
    };
}

/// Theme and key bindings: everything both the window and headless
/// screenshots need.
fn setup(cx: &mut App, dark: bool, font_size: f32) {
    gpui_kit::init(cx);
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    Theme::update(cx, |t| {
        t.font_size = px(font_size);
        t.mono_font_size = px(font_size - 1.);
        t.radius = px(6.);
        // A database-blue accent.
        let (primary, hover) = if dark { (0x3b82f6, 0x60a5fa) } else { (0x2563eb, 0x1d4ed8) };
        t.primary = c(primary);
        t.primary_hover = c(hover);
        t.primary_active = c(hover);
        t.button_primary = c(primary);
        t.button_primary_hover = c(hover);
        t.button_primary_active = c(hover);
        t.ring = c(primary);
        t.caret = c(primary);
        t.link = c(primary);
    });
    cx.bind_keys([
        KeyBinding::new(key!("enter"), RunQuery, Some("QueryTab")),
        KeyBinding::new("f5", RunQuery, Some("QueryTab")),
        KeyBinding::new(key!("k"), AskAiInEditor, Some("QueryTab")),
        KeyBinding::new(key!("t"), NewQuery, Some("Workspace")),
        KeyBinding::new(key!("w"), CloseTab, Some("Workspace")),
        KeyBinding::new(key!("shift-a"), ToggleAi, Some("Workspace")),
        KeyBinding::new(key!("r"), Refresh, Some("Workspace")),
        KeyBinding::new(key!("c"), CopySelection, Some("ResultGrid")),
        KeyBinding::new(key!("h"), ToggleHistory, Some("Workspace")),
        KeyBinding::new(key!("="), ZoomIn, Some("Workspace")),
        KeyBinding::new(key!("+"), ZoomIn, Some("Workspace")),
        KeyBinding::new(key!("-"), ZoomOut, Some("Workspace")),
        KeyBinding::new(key!("0"), ZoomReset, Some("Workspace")),
        KeyBinding::new(key!("s"), SaveChanges, Some("DataTab")),
        KeyBinding::new("f2", EditCell, Some("DataTab")),
    ]);
}

pub fn run(url: Option<String>, connect: Option<String>) -> anyhow::Result<()> {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx: &mut App| {
            let dark = !matches!(
                cx.window_appearance(),
                gpui_kit::WindowAppearance::Light | gpui_kit::WindowAppearance::VibrantLight
            );
            setup(cx, dark, settings::load().font_size());
            let bounds = Bounds::centered(None, size(px(1400.), px(880.)), cx);
            let opened = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Tabula".into()),
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(800.), px(500.))),
                    app_id: Some("tabula".into()),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| Workspace::new(url.clone(), connect.clone(), true, window, cx)),
            );
            if let Err(e) = opened {
                eprintln!("couldn't open the window: {e}");
                cx.quit();
            }
            cx.on_window_closed(|cx, _| cx.quit()).detach();
            cx.activate(true);
        });
    Ok(())
}

/// Render the main views headless (no display needed) to PNGs, dark and
/// light. With `url`, connect first so the views show real data.
pub fn screenshots(dir: PathBuf, url: Option<String>, connect: Option<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(&dir)?;
    for dark in [true, false] {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(gpui_kit::assets::AllAssets),
            gpui_kit::platform::current_headless_renderer,
        );
        cx.update(|cx| setup(cx, dark, settings::DEFAULT_FONT_SIZE));
        let (win, ws) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1400.), px(880.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| Workspace::new(url.clone(), connect.clone(), false, window, cx)),
            )
        })?;
        let mode = if dark { "dark" } else { "light" };
        let settle = |cx: &mut HeadlessAppContext| -> anyhow::Result<()> {
            // Database work runs on tokio; give it time to come back.
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                cx.run_until_parked();
                cx.update_window(win, |_, window, cx| window.render_frame(cx))?;
            }
            Ok(())
        };
        let shoot = |cx: &mut HeadlessAppContext, name: &str| -> anyhow::Result<()> {
            let p = dir.join(format!("{name}-{mode}.png"));
            cx.capture_screenshot(win)?.save(&p)?;
            println!("saved {}", p.display());
            Ok(())
        };
        settle(&mut cx)?;
        shoot(&mut cx, "start")?;
        if url.is_none() && connect.is_none() {
            continue;
        }
        for step in workspace::ShotStep::ALL {
            cx.update_window(win, |_, window, cx| {
                ws.update(cx, |w, cx| w.shot(step, window, cx))
            })?;
            settle(&mut cx)?;
            if !matches!(step, workspace::ShotStep::EditOpen) {
                shoot(&mut cx, step.name())?;
            }
        }
    }
    Ok(())
}
