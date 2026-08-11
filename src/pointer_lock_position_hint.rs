use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use sctk::reexports::client::{Connection, QueueHandle};
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1, zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use sctk::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState};
use sctk::seat::relative_pointer::{
    RelativeMotionEvent, RelativePointerHandler, RelativePointerState,
};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_output, delegate_pointer, delegate_pointer_constraints,
    delegate_registry, delegate_relative_pointer, delegate_seat, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

const WINDOW_W: u32 = 640;
const WINDOW_H: u32 = 480;
const DOT_RADIUS: f64 = 8.0;

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm is not available");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("pointer_lock_position_hint");
    window.set_app_id("pointer_lock_position_hint");
    window.set_min_size(Some((256, 256)));
    window.commit();

    let pool = SlotPool::new(WINDOW_W as usize * WINDOW_H as usize * 4, &shm)
        .expect("Failed to create pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        relative_pointer_state: RelativePointerState::bind(&globals, &qh),
        pointer_constraints_state: PointerConstraintsState::bind(&globals, &qh),
        shm,

        exit: false,
        pool,
        width: WINDOW_W,
        height: WINDOW_H,
        buffer: None,
        frame_pending: false,
        dirty: true,
        window,

        pointer: None,
        relative_pointer: None,
        locked_pointer: None,
        lock_active: false,
        dot: (WINDOW_W as f64 / 2.0, WINDOW_H as f64 / 2.0),
    };

    println!("=== pointer_lock_position_hint test ===");
    println!("Move the pointer into the window to activate the persistent pointer lock.");
    println!("Left click to toggle the pointer lock off and on.");
    println!("The system cursor remains visible at the lock position.");
    println!(
        "The dot follows relative pointer motion, and every update immediately sets and commits a cursor position hint."
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
    seat_state: SeatState,
    output_state: OutputState,
    relative_pointer_state: RelativePointerState,
    pointer_constraints_state: PointerConstraintsState,
    shm: Shm,

    exit: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    frame_pending: bool,
    dirty: bool,
    window: Window,

    pointer: Option<wl_pointer::WlPointer>,
    relative_pointer: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    locked_pointer: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    lock_active: bool,
    dot: (f64, f64),
}

impl App {
    fn clamp_dot(&mut self) {
        self.dot.0 = self.dot.0.clamp(0.0, self.width.saturating_sub(1) as f64);
        self.dot.1 = self.dot.1.clamp(0.0, self.height.saturating_sub(1) as f64);
    }

    fn set_cursor_position_hint(&self) {
        if let Some(locked_pointer) = &self.locked_pointer {
            locked_pointer.set_cursor_position_hint(self.dot.0, self.dot.1);
        }
    }

    fn toggle_pointer_lock(&mut self, qh: &QueueHandle<Self>) {
        if let Some(locked_pointer) = self.locked_pointer.take() {
            locked_pointer.destroy();
            self.lock_active = false;
            self.visual_changed(qh);
            println!("Pointer lock disabled");
            return;
        }

        let Some(pointer) = &self.pointer else {
            return;
        };
        let locked_pointer = self
            .pointer_constraints_state
            .lock_pointer(
                self.window.wl_surface(),
                pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                qh,
            )
            .expect("zwp_pointer_constraints_v1 is not available");
        self.locked_pointer = Some(locked_pointer);
        self.set_cursor_position_hint();
        self.window.commit();
        println!("Pointer lock requested");
    }

    fn visual_changed(&mut self, qh: &QueueHandle<Self>) {
        self.dirty = true;

        // The hint is double-buffered surface state. Send and commit it immediately for every
        // update instead of waiting until the next opportunity to redraw the dot.
        self.set_cursor_position_hint();
        self.window.commit();

        if !self.frame_pending {
            self.draw(qh);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if !self.dirty || self.width == 0 || self.height == 0 {
            return;
        }

        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;
        let dot = self.dot;
        let lock_active = self.lock_active;

        let buffer = self.buffer.get_or_insert_with(|| {
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
                let (new_buffer, canvas) = self
                    .pool
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

        for (i, pixel) in canvas.chunks_exact_mut(4).enumerate() {
            let x = (i % width as usize) as f64;
            let y = (i / width as usize) as f64;
            let distance_squared = (x - dot.0).powi(2) + (y - dot.1).powi(2);
            let border = x < 4.0
                || y < 4.0
                || x >= width.saturating_sub(4) as f64
                || y >= height.saturating_sub(4) as f64;

            let color: u32 = if distance_squared <= DOT_RADIUS.powi(2) {
                0xFF_FF_FF_FF
            } else if distance_squared <= (DOT_RADIUS + 2.0).powi(2) {
                0xFF_E0_30_30
            } else if border {
                if lock_active {
                    0xFF_30_C0_60
                } else {
                    0xFF_D0_90_20
                }
            } else {
                0xFF_18_24_38
            };
            pixel.copy_from_slice(&color.to_le_bytes());
        }

        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(surface).expect("buffer attach");
        surface.frame(qh, surface.clone());
        self.set_cursor_position_hint();
        surface.commit();

        self.dirty = false;
        self.frame_pending = true;
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
        self.frame_pending = false;
        self.draw(qh);
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
        let new_width = configure.new_size.0.map(|v| v.get()).unwrap_or(WINDOW_W);
        let new_height = configure.new_size.1.map(|v| v.get()).unwrap_or(WINDOW_H);

        if new_width != self.width || new_height != self.height {
            self.width = new_width;
            self.height = new_height;
            self.buffer = None;
            self.clamp_dot();
        }

        self.visual_changed(qh);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer || self.pointer.is_some() {
            return;
        }

        let pointer = self
            .seat_state
            .get_pointer(qh, &seat)
            .expect("Failed to create pointer");
        let relative_pointer = self
            .relative_pointer_state
            .get_relative_pointer(&pointer, qh)
            .expect("zwp_relative_pointer_manager_v1 is not available");
        let locked_pointer = self
            .pointer_constraints_state
            .lock_pointer(
                self.window.wl_surface(),
                &pointer,
                None,
                zwp_pointer_constraints_v1::Lifetime::Persistent,
                qh,
            )
            .expect("zwp_pointer_constraints_v1 is not available");

        self.pointer = Some(pointer);
        self.relative_pointer = Some(relative_pointer);
        self.locked_pointer = Some(locked_pointer);
        self.set_cursor_position_hint();
        self.window.commit();
        println!("Pointer lock requested");
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer {
            return;
        }

        if let Some(locked_pointer) = self.locked_pointer.take() {
            locked_pointer.destroy();
        }
        if let Some(relative_pointer) = self.relative_pointer.take() {
            relative_pointer.destroy();
        }
        if let Some(pointer) = self.pointer.take() {
            pointer.release();
        }
        self.lock_active = false;
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.dot = event.position;
                    self.clamp_dot();
                    self.visual_changed(qh);
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.toggle_pointer_lock(qh);
                }
                _ => {}
            }
        }
    }
}

impl PointerConstraintsHandler for App {
    fn confined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn unconfined(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &zwp_confined_pointer_v1::ZwpConfinedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
    }

    fn locked(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
        self.lock_active = true;
        self.visual_changed(qh);
        println!("Pointer lock activated");
    }

    fn unlocked(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        _: &wl_surface::WlSurface,
        _: &wl_pointer::WlPointer,
    ) {
        self.lock_active = false;
        self.visual_changed(qh);
        println!("Pointer lock deactivated");
    }
}

impl RelativePointerHandler for App {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        _: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        self.dot.0 += event.delta.0;
        self.dot.1 += event.delta.1;
        self.clamp_dot();
        self.visual_changed(qh);
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

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_pointer_constraints!(App);
delegate_relative_pointer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState,];
}
