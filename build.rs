//! Renders `assets/icon.svg` to the app icon: an `.ico` embedded in the
//! Windows executable (which also gives every window its title-bar and
//! taskbar icon) and a PNG the app shows on its welcome screen.

use std::path::Path;

fn render(tree: &resvg::usvg::Tree, size: u32) -> resvg::tiny_skia::Pixmap {
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("icon size is not zero");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.svg");
    println!("cargo:rerun-if-changed=build.rs");
    let out = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out = Path::new(&out);

    let svg = std::fs::read("assets/icon.svg").expect("assets/icon.svg");
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default())
        .expect("assets/icon.svg is valid SVG");

    render(&tree, 256).save_png(out.join("icon.png")).expect("write icon.png");

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 24, 32, 48, 64, 256] {
        let rgba: Vec<u8> = render(&tree, size)
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode icon"));
    }
    let ico_path = out.join("icon.ico");
    dir.write(std::fs::File::create(&ico_path).expect("create icon.ico")).expect("write icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI loads icon resource 1 for its windows.
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id(&ico_path.to_string_lossy(), "1");
        if let Err(e) = res.compile() {
            // No resource compiler: the app still builds, without an icon.
            println!("cargo:warning=couldn't embed the app icon: {e}");
        }
    }
}
