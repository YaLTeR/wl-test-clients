use std::time::{Duration, Instant};

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::presentation_time::{PresentTime, PresentationTimeHandler, PresentationTimeState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use sctk::reexports::client::{Connection, QueueHandle, WEnum};
use sctk::reexports::protocols::wp::presentation_time::client::wp_presentation_feedback;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_output, delegate_presentation_time, delegate_registry,
    delegate_shm, delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("create event loop");
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let presentation_time_state = PresentationTimeState::bind(&globals, &qh);
    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("create pool");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("window_frame_callbacks_presentation");
    window.set_min_size(Some((1, 1)));
    window.commit();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        presentation_time_state,
        window,
        pool,
        buffers: [None, None],
        current_buffer: 0,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        first_configure: true,
        exit: false,
        frame_count: 0,
        last_frame: None,
        last_presentation: None,
    };

    println!("=== window_frame_callbacks_presentation test ===");
    println!(
        "Continuously committing nonempty damage with frame callbacks and presentation feedback."
    );

    while !app.exit {
        event_loop.dispatch(None, &mut app).unwrap();
    }
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    presentation_time_state: PresentationTimeState,
    window: Window,
    pool: SlotPool,
    buffers: [Option<Buffer>; 2],
    current_buffer: usize,
    width: u32,
    height: u32,
    first_configure: bool,
    exit: bool,
    frame_count: u64,
    last_frame: Option<Instant>,
    last_presentation: Option<Duration>,
}

impl App {
    fn render_buffer(pool: &mut SlotPool, buffer: &mut Option<Buffer>, width: u32, height: u32) {
        let stride = width as i32 * 4;
        let buffer = buffer.get_or_insert_with(|| {
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
                    .expect("create replacement buffer");
                *buffer = new_buffer;
                canvas
            }
        };

        for (i, pixel) in canvas.chunks_exact_mut(4).enumerate() {
            let x = i % width as usize;
            let color: u32 = if x < width as usize / 2 {
                0xFF_10_20_40
            } else {
                0xFF_40_10_10
            };
            pixel.copy_from_slice(&color.to_le_bytes());
        }
    }

    fn render_both_buffers(&mut self) {
        let mut first = self.buffers[0].take();
        let mut second = self.buffers[1].take();
        Self::render_buffer(&mut self.pool, &mut first, self.width, self.height);
        Self::render_buffer(&mut self.pool, &mut second, self.width, self.height);
        self.buffers = [first, second];
    }

    fn submit_frame(&mut self, qh: &QueueHandle<Self>, full_damage: bool) {
        let surface = self.window.wl_surface();
        self.buffers[self.current_buffer]
            .as_ref()
            .expect("buffer not rendered")
            .attach_to(surface)
            .expect("attach buffer");

        let damage_width = if full_damage {
            self.width as i32
        } else {
            (self.width as i32 / 2).max(1)
        };
        surface.damage_buffer(0, 0, damage_width, self.height as i32);
        surface.frame(qh, surface.clone());
        self.presentation_time_state
            .feedback(surface, qh)
            .expect("wp_presentation not available");
        surface.commit();
        self.current_buffer ^= 1;
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

    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        let now = Instant::now();
        self.frame_count += 1;
        if let Some(last) = self.last_frame {
            println!(
                "frame #{}: {:.3} ms since last",
                self.frame_count,
                now.duration_since(last).as_secs_f64() * 1000.0
            );
        } else {
            println!("frame #{}: first frame callback", self.frame_count);
        }
        self.last_frame = Some(now);
        self.submit_frame(qh, false);
    }

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
        qh: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let width = configure
            .new_size
            .0
            .map(|v| v.get())
            .unwrap_or(DEFAULT_WIDTH);
        let height = configure
            .new_size
            .1
            .map(|v| v.get())
            .unwrap_or(DEFAULT_HEIGHT);
        let size_changed = width != self.width || height != self.height;
        self.width = width;
        self.height = height;

        if self.first_configure || size_changed {
            self.first_configure = false;
            self.buffers = [None, None];
            self.current_buffer = 0;
            self.render_both_buffers();
            println!("configured: {}x{}", self.width, self.height);
            self.submit_frame(qh, true);
        }
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time_state
    }

    fn presented(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        _: &wl_surface::WlSurface,
        _: Vec<wl_output::WlOutput>,
        time: PresentTime,
        refresh: u32,
        _: u64,
        _: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let presentation = Duration::new(time.tv_sec, time.tv_nsec);
        if let Some(last) = self.last_presentation {
            let delta = presentation.as_nanos() as i128 - last.as_nanos() as i128;
            let refresh = i128::from(refresh);
            println!(
                "presentation: delta {:.3} ms, refresh {:.3} ms, difference {:+.3} ms",
                delta as f64 / 1_000_000.0,
                refresh as f64 / 1_000_000.0,
                (delta - refresh) as f64 / 1_000_000.0,
            );
        } else {
            println!("presentation: first presentation");
        }
        self.last_presentation = Some(presentation);
    }

    fn discarded(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        _: &wl_surface::WlSurface,
    ) {
        println!("presentation: discarded");
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

delegate_compositor!(App);
delegate_output!(App);
delegate_presentation_time!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState,];
}
