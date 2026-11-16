//! `maximize-check show EDGE [--fullscreen] [--auto-hide]`: a panel-like layer bar on EDGE
//! (bottom, top, left or right) that reserves its width, and a flat-colour window that asks to be
//! maximized (or fullscreen) before its first commit. Prints `configure W H` each time the window
//! is drawn at a size the compositor gave it. With `--auto-hide` the bar reserves nothing and
//! asks the compositor to hide it off EDGE, as the Kora panel does, printing `visible 0|1`.
//!
//! `maximize-check rect SHOT [bar]`: `x y w h` of the window's (or bar's) colour in a capture (PPM).

use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsFd,
    process::exit,
};

use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

#[allow(non_upper_case_globals, non_camel_case_types, clippy::all)]
mod auto_hide {
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../resources/protocols/layer-auto-hide.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("../../resources/protocols/layer-auto-hide.xml");
}
use auto_hide::{layer_auto_hide_manager_v1, layer_auto_hide_v1};

const BAR: [u8; 3] = [0x20, 0xc0, 0x40];
const WINDOW: [u8; 3] = [0x20, 0x50, 0xe0];
const BAR_SIZE: u32 = 52;

struct App {
    qh: QueueHandle<App>,
    shm: wl_shm::WlShm,
    bar: wl_surface::WlSurface,
    bar_drawn: bool,
    vertical: bool,
    window: wl_surface::WlSurface,
    pending: (i32, i32),
    drawn: (i32, i32),
    files: Vec<File>,
}

impl App {
    fn draw(&mut self, surface: &wl_surface::WlSurface, (w, h): (u32, u32), rgb: [u8; 3]) {
        let dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR");
        let path = format!(
            "{dir}/maximize-check-{}-{}",
            std::process::id(),
            self.files.len()
        );
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("shm file");
        std::fs::remove_file(&path).expect("unlink shm file");
        let pixel = [rgb[2], rgb[1], rgb[0], 0xff];
        let row: Vec<u8> = pixel.iter().copied().cycle().take(w as usize * 4).collect();
        for _ in 0..h {
            file.write_all(&row).expect("shm write");
        }
        let pool = self
            .shm
            .create_pool(file.as_fd(), (w * h * 4) as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            w as i32 * 4,
            wl_shm::Format::Xrgb8888,
            &self.qh,
            (),
        );
        pool.destroy();
        // One file per buffer; an older one may still be on screen.
        self.files.push(file);
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, w as i32, h as i32);
        surface.commit();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("show") if args.len() >= 2 => show(
            &args[1],
            args.iter().any(|a| a == "--fullscreen"),
            args.iter().any(|a| a == "--auto-hide"),
        ),
        Some("rect") if args.len() == 2 => rect(&args[1], WINDOW),
        Some("rect") if args.len() == 3 && args[2] == "bar" => rect(&args[1], BAR),
        _ => {
            eprintln!(
                "usage: maximize-check show bottom|top|left|right [--fullscreen] [--auto-hide] | rect SHOT [bar]"
            );
            exit(2);
        }
    }
}

fn show(edge: &str, fullscreen: bool, auto_hide: bool) {
    use layer_auto_hide_v1::Edge;
    use zwlr_layer_surface_v1::Anchor;
    let (anchor, vertical, hide_edge) = match edge {
        "bottom" => (
            Anchor::Bottom | Anchor::Left | Anchor::Right,
            false,
            Edge::Bottom,
        ),
        "top" => (Anchor::Top | Anchor::Left | Anchor::Right, false, Edge::Top),
        "left" => (
            Anchor::Left | Anchor::Top | Anchor::Bottom,
            true,
            Edge::Left,
        ),
        "right" => (
            Anchor::Right | Anchor::Top | Anchor::Bottom,
            true,
            Edge::Right,
        ),
        _ => {
            eprintln!("unknown edge {edge}");
            exit(2);
        }
    };
    let conn = Connection::connect_to_env().expect("no Wayland display");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).expect("compositor");
    let shm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 2..=6, ()).expect("xdg_wm_base");
    let layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
        globals.bind(&qh, 3..=4, ()).expect("layer shell");

    let bar = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &bar,
        None,
        if auto_hide {
            zwlr_layer_shell_v1::Layer::Overlay
        } else {
            zwlr_layer_shell_v1::Layer::Top
        },
        "maximize-check-bar".into(),
        &qh,
        (),
    );
    layer.set_anchor(anchor);
    if vertical {
        layer.set_size(BAR_SIZE, 0);
    } else {
        layer.set_size(0, BAR_SIZE);
    }
    if auto_hide {
        // Always mode with a 4px edge zone, reserving nothing.
        let manager: layer_auto_hide_manager_v1::LayerAutoHideManagerV1 =
            globals.bind(&qh, 1..=2, ()).expect("layer auto-hide");
        manager.get_auto_hide(&bar, &qh, ()).set_auto_hide(
            hide_edge,
            4,
            layer_auto_hide_v1::Mode::Always,
        );
        layer.set_exclusive_zone(0);
    } else {
        layer.set_exclusive_zone(BAR_SIZE as i32);
    }
    bar.commit();

    let window = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&window, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("maximize-check".into());
    toplevel.set_app_id("maximize-check".into());
    if fullscreen {
        toplevel.set_fullscreen(None);
    } else {
        toplevel.set_maximized();
    }
    window.commit();

    let mut app = App {
        qh,
        shm,
        bar,
        bar_drawn: false,
        vertical,
        window,
        pending: (0, 0),
        drawn: (0, 0),
        files: Vec::new(),
    };
    loop {
        queue.blocking_dispatch(&mut app).expect("dispatch");
    }
}

/// The bounding box of `colour` in a binary PPM.
fn rect(path: &str, colour: [u8; 3]) {
    let mut data = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut data))
        .expect("read the shot");
    let (width, pixels) = ppm(&data);
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (i, p) in pixels.chunks_exact(3).enumerate() {
        if p == colour {
            let (x, y) = (i % width, i / width);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x0 == usize::MAX {
        println!("none");
    } else {
        println!("{x0} {y0} {} {}", x1 - x0 + 1, y1 - y0 + 1);
    }
}

fn ppm(data: &[u8]) -> (usize, &[u8]) {
    let mut fields = Vec::new();
    let mut at = 0;
    while fields.len() < 4 {
        while data[at].is_ascii_whitespace() {
            at += 1;
        }
        if data[at] == b'#' {
            while data[at] != b'\n' {
                at += 1;
            }
            continue;
        }
        let start = at;
        while !data[at].is_ascii_whitespace() {
            at += 1;
        }
        fields.push(String::from_utf8_lossy(&data[start..at]).into_owned());
    }
    assert!(fields[3] == "255", "not an 8-bit PPM");
    (fields[1].parse().expect("PPM width"), &data[at + 1..])
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        app: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if app.bar_drawn {
                    app.bar.commit();
                    return;
                }
                app.bar_drawn = true;
                let size = if app.vertical {
                    (BAR_SIZE, height.max(1))
                } else {
                    (width.max(1), BAR_SIZE)
                };
                let bar = app.bar.clone();
                app.draw(&bar, size, BAR);
            }
            zwlr_layer_surface_v1::Event::Closed => exit(0),
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(
        app: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                app.pending = (width, height);
            }
            xdg_toplevel::Event::Close => exit(0),
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        app: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg.ack_configure(serial);
        let size = if app.pending == (0, 0) {
            (640, 400)
        } else {
            app.pending
        };
        if size == app.drawn {
            app.window.commit();
            return;
        }
        app.drawn = size;
        let window = app.window.clone();
        app.draw(&window, (size.0 as u32, size.1 as u32), WINDOW);
        println!("configure {} {}", size.0, size.1);
        std::io::stdout().flush().ok();
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(App: ignore layer_auto_hide_manager_v1::LayerAutoHideManagerV1);

impl Dispatch<layer_auto_hide_v1::LayerAutoHideV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &layer_auto_hide_v1::LayerAutoHideV1,
        event: layer_auto_hide_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let layer_auto_hide_v1::Event::VisibilityChanged { visible } = event;
        println!("visible {visible}");
        std::io::stdout().flush().ok();
    }
}
