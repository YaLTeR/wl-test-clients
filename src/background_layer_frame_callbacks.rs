use std::time::Instant;

use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

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
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");

    let surface = compositor.create_surface(&qh);
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Background,
        Some("background_layer_frame_callbacks"),
        None,
    );
    layer_surface.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,

        exit: false,
        first_configure: true,
        pool,
        width: 0,
        height: 0,
        buffers: [None, None],
        current_buf: 0,
        layer_surface,

        last_frame: None,
        frame_count: 0,
    };

    println!("=== background_layer_frame_callbacks test ===");
    println!("Background layer surface, anchored to all edges (fullscreen).");
    println!("Looping over frame callbacks; printing time since last frame.");

    loop {
        event_loop.dispatch(None, &mut app).unwrap();
        if app.exit {
            println!("exiting");
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
    width: u32,
    height: u32,
    buffers: [Option<Buffer>; 2],
    current_buf: usize,
    layer_surface: LayerSurface,

    last_frame: Option<Instant>,
    frame_count: u64,
}

impl App {
    fn render_buffer(pool: &mut SlotPool, buf: &mut Option<Buffer>, width: u32, height: u32) {
        let stride = width as i32 * 4;

        let buffer = buf.get_or_insert_with(|| {
            pool.create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("create buffer")
            .0
        });

        let canvas = match pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (new_buffer, canvas) = pool
                    .create_buffer(
                        width as i32,
                        height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer");
                *buffer = new_buffer;
                canvas
            }
        };

        // Left half dark blue, right half dark red, so the damage region is visually obvious.
        let half_w = width as usize / 2;
        for (i, chunk) in canvas.chunks_exact_mut(4).enumerate() {
            let x = i % width as usize;
            let color: u32 = if x < half_w {
                0xFF_10_20_40 // dark blue (left, damaged each frame)
            } else {
                0xFF_40_10_10 // dark red (right, never damaged)
            };
            chunk.copy_from_slice(&color.to_le_bytes());
        }
    }

    fn render_both_buffers(&mut self) {
        // Take buffers out so we can borrow pool mutably.
        let mut buf0 = self.buffers[0].take();
        let mut buf1 = self.buffers[1].take();
        Self::render_buffer(&mut self.pool, &mut buf0, self.width, self.height);
        Self::render_buffer(&mut self.pool, &mut buf1, self.width, self.height);
        self.buffers[0] = buf0;
        self.buffers[1] = buf1;
    }

    /// Attach the current buffer, damage the left half, and commit with a frame callback.
    fn submit_frame(&mut self, qh: &QueueHandle<Self>) {
        let buffer = self.buffers[self.current_buf]
            .as_ref()
            .expect("buffer not rendered");
        buffer
            .attach_to(self.layer_surface.wl_surface())
            .expect("buffer attach");

        let surface = self.layer_surface.wl_surface();
        surface.damage_buffer(0, 0, self.width as i32 / 2, self.height as i32);
        surface.frame(qh, surface.clone());
        surface.commit();

        // Flip to the other buffer for next frame.
        self.current_buf ^= 1;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        let now = Instant::now();
        self.frame_count += 1;

        if let Some(last) = self.last_frame {
            let dt = now.duration_since(last);
            println!(
                "frame #{}: {:.2} ms since last",
                self.frame_count,
                dt.as_secs_f64() * 1000.0
            );
        } else {
            println!("frame #{}: first frame callback", self.frame_count);
        }

        self.last_frame = Some(now);

        self.submit_frame(_qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
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

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let new_width = configure.new_size.0;
        let new_height = configure.new_size.1;

        if new_width == 0 || new_height == 0 {
            return;
        }

        let size_changed = new_width != self.width || new_height != self.height;
        self.width = new_width;
        self.height = new_height;

        if size_changed || self.first_configure {
            self.first_configure = false;
            self.buffers = [None, None];
            self.current_buf = 0;
            self.render_both_buffers();
            println!(
                "configured: {}x{}, submitting initial buffer and requesting first frame",
                self.width, self.height
            );
            // Attach with full damage for the initial frame.
            let buffer = self.buffers[self.current_buf].as_ref().unwrap();
            buffer
                .attach_to(self.layer_surface.wl_surface())
                .expect("buffer attach");
            let surface = self.layer_surface.wl_surface();
            surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
            surface.frame(_qh, surface.clone());
            surface.commit();
            self.current_buf ^= 1;
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState,];
}
