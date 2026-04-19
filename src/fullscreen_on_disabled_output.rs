use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
};

const WINDOW_W: u32 = 400;
const WINDOW_H: u32 = 300;

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("fullscreen_on_disabled_output");
    window.set_min_size(Some((256, 256)));
    window.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let registry_state = RegistryState::new(&globals);

    // Bind all wl_output globals present at startup. We keep the proxies
    // ourselves instead of using sctk's OutputState, because OutputState
    // destroys the wl_output proxy on global removal — and this test is
    // specifically about the window of time between a wl_output global
    // being "disabled" (global removed) and the proxy being destroyed.
    let output_names: Vec<u32> = registry_state
        .globals_by_interface("wl_output")
        .map(|g| g.name)
        .collect();
    let mut outputs = Vec::with_capacity(output_names.len());
    for name in output_names {
        let output: wl_output::WlOutput = registry_state
            .bind_specific(&qh, name, 1..=4, name)
            .expect("bind wl_output");
        outputs.push((name, output));
    }

    let mut app = App {
        registry_state,
        // OutputState is only here to satisfy sctk's CompositorState bound
        // (it requires OutputHandler on `App`). We do not route registry
        // events into it, and our outputs use a different user_data type,
        // so it will stay empty and not destroy our proxies.
        output_state: OutputState::new(&globals, &qh),
        shm,

        exit: false,
        first_configure: true,
        pool,
        window_width: WINDOW_W,
        window_height: WINDOW_H,
        window_buffer: None,
        window,

        outputs,
    };

    println!("=== fullscreen_on_disabled_output test ===");
    println!(
        "Bound {} wl_output global(s) at startup.",
        app.outputs.len()
    );
    println!(
        "On wl_output global removal, the window will set_fullscreen on \
         that (now-disabled) output."
    );

    loop {
        event_loop.dispatch(None, &mut app).unwrap();
        if app.exit {
            break;
        }
    }
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    window_width: u32,
    window_height: u32,
    window_buffer: Option<Buffer>,
    window: Window,

    // (registry name, bound proxy). Proxies of removed globals are kept
    // alive so set_fullscreen can still reference them.
    outputs: Vec<(u32, wl_output::WlOutput)>,
}

impl App {
    fn draw_window(&mut self) {
        let width = self.window_width;
        let height = self.window_height;
        let stride = width as i32 * 4;

        let buffer = self.window_buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(
                    width as i32,
                    height as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                )
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(
                        width as i32,
                        height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer");
                *buffer = second_buffer;
                canvas
            }
        };

        canvas.chunks_exact_mut(4).for_each(|chunk| {
            let color: u32 = 0xFF_40_80_C0;
            chunk.copy_from_slice(&color.to_le_bytes());
        });

        self.window
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.window.wl_surface())
            .expect("buffer attach");
        self.window.commit();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let new_width = configure.new_size.0.map(|v| v.get()).unwrap_or(WINDOW_W);
        let new_height = configure.new_size.1.map(|v| v.get()).unwrap_or(WINDOW_H);

        let size_changed = new_width != self.window_width || new_height != self.window_height;
        self.window_width = new_width;
        self.window_height = new_height;

        if size_changed || self.first_configure {
            self.first_configure = false;
            self.window_buffer = None;
            self.draw_window();
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl Dispatch<wl_output::WlOutput, u32> for App {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    fn runtime_add_global(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        name: u32,
        interface: &str,
        _version: u32,
    ) {
        if interface == "wl_output" {
            let output: wl_output::WlOutput = self
                .registry_state
                .bind_specific(qh, name, 1..=4, name)
                .expect("bind wl_output");
            self.outputs.push((name, output));
            println!("wl_output global added: name={name}");
        }
    }

    fn runtime_remove_global(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        name: u32,
        interface: &str,
    ) {
        if interface == "wl_output" {
            if let Some(pos) = self.outputs.iter().position(|(n, _)| *n == name) {
                let (_, output) = &self.outputs[pos];
                println!("wl_output global removed: name={name} — calling set_fullscreen on it");
                self.window.set_fullscreen(Some(output));
                // Keep the proxy alive so the compositor still sees a
                // valid (but disabled) output object referenced by the
                // request.
            }
        }
    }
}
