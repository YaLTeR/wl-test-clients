use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1;
use smithay_client_toolkit::{
    background_effect::{BackgroundEffectHandler, BackgroundEffectState},
    compositor::{CompositorHandler, CompositorState},
    delegate_background_effect, delegate_compositor, delegate_keyboard, delegate_output,
    delegate_pointer, delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor,
    delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
    subcompositor::SubcompositorState,
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{
        wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_subsurface, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};

const WINDOW_W: u32 = 400;
const WINDOW_H: u32 = 300;
const SUB_W: u32 = 200;
const SUB_H: u32 = 200;

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
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .expect("wl_subcompositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");
    let bg_effect = BackgroundEffectState::new(&globals, &qh);

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("subsurface_no_geometry");
    window.set_min_size(Some((256, 256)));
    // No geometry set on the window — that's the whole point of this test.
    window.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let window_bg_effect = bg_effect
        .get_background_effect(window.wl_surface(), &qh)
        .ok();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        subcompositor,
        shm,
        bg_effect,

        exit: false,
        first_configure: true,
        pool,
        window_width: WINDOW_W,
        window_height: WINDOW_H,
        window_buffer: None,
        window,
        window_bg_effect,
        keyboard: None,
        pointer: None,

        subsurface: None,
    };

    println!("=== subsurface_no_geometry test ===");
    println!("Main surface: semitransparent, no geometry, full-surface blur.");
    println!("Click to toggle a 200x200 subsurface centered at click position.");
    println!("The subsurface may extend outside the main surface bounds.");
    println!("Q/Escape to quit.");

    loop {
        event_loop
            .dispatch(Duration::from_millis(16), &mut app)
            .unwrap();
        if app.exit {
            println!("exiting");
            break;
        }
    }
}

struct SubsurfaceState {
    subsurface: wl_subsurface::WlSubsurface,
    surface: wl_surface::WlSurface,
    #[allow(dead_code)]
    buffer: Option<Buffer>,
    bg_effect_surface: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    shm: Shm,
    bg_effect: BackgroundEffectState,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    window_width: u32,
    window_height: u32,
    window_buffer: Option<Buffer>,
    window: Window,
    window_bg_effect: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    subsurface: Option<SubsurfaceState>,
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

        // Semitransparent white tint so blur is visible through.
        // Premultiplied alpha: A=0x60, RGB=0x60.
        canvas.chunks_exact_mut(4).for_each(|chunk| {
            let color: u32 = 0x60_60_60_60;
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

    fn update_window_blur_region(&self, qh: &QueueHandle<Self>) {
        if let Some(ref bg) = self.window_bg_effect {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            region.add(0, 0, self.window_width as i32, self.window_height as i32);
            bg.set_blur_region(Some(&region));
            region.destroy();
        }
    }

    fn toggle_subsurface(&mut self, click_x: i32, click_y: i32, qh: &QueueHandle<Self>) {
        if self.subsurface.is_some() {
            self.destroy_subsurface();
        } else {
            self.create_subsurface(click_x, click_y, qh);
        }
    }

    fn create_subsurface(&mut self, click_x: i32, click_y: i32, qh: &QueueHandle<Self>) {
        let (subsurface, surface) = self
            .subcompositor
            .create_subsurface(self.window.wl_surface().clone(), qh);

        // Center the 200x200 subsurface at the click position.
        // This may place it partially or fully outside the main surface.
        let x = click_x - (SUB_W as i32) / 2;
        let y = click_y - (SUB_H as i32) / 2;
        subsurface.set_position(x, y);
        // Desync so the subsurface commits independently.
        subsurface.set_desync();

        println!(
            "Created subsurface at ({}, {}), size {}x{} — extends outside: {}",
            x,
            y,
            SUB_W,
            SUB_H,
            x < 0
                || y < 0
                || (x + SUB_W as i32) > self.window_width as i32
                || (y + SUB_H as i32) > self.window_height as i32
        );

        // Set blur on the subsurface covering its full size.
        let bg_surface = self.bg_effect.get_background_effect(&surface, qh).ok();
        if let Some(ref bg) = bg_surface {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            region.add(0, 0, SUB_W as i32, SUB_H as i32);
            bg.set_blur_region(Some(&region));
            region.destroy();
        }

        // Draw the subsurface buffer.
        let stride = SUB_W as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(SUB_W as i32, SUB_H as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");

        // Semitransparent green tint (premultiplied alpha).
        // A=0x60, R=0x00, G=0x60, B=0x00.
        canvas.chunks_exact_mut(4).for_each(|chunk| {
            let color: u32 = 0x60_00_60_00;
            chunk.copy_from_slice(&color.to_le_bytes());
        });

        surface.damage_buffer(0, 0, SUB_W as i32, SUB_H as i32);
        buffer.attach_to(&surface).expect("buffer attach");
        surface.commit();

        self.subsurface = Some(SubsurfaceState {
            subsurface,
            surface,
            buffer: Some(buffer),
            bg_effect_surface: bg_surface,
        });

        // Commit the parent to pick up the new subsurface.
        self.window.wl_surface().commit();
    }

    fn destroy_subsurface(&mut self) {
        if let Some(mut state) = self.subsurface.take() {
            if let Some(bg) = state.bg_effect_surface.take() {
                bg.destroy();
            }
            state.subsurface.destroy();
            state.surface.destroy();
            // Commit parent to apply subsurface removal.
            self.window.wl_surface().commit();
            println!("Destroyed subsurface");
        }
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

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
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
            self.update_window_blur_region(qh);
            self.draw_window();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }

        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to create pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kbd) = self.keyboard.take() {
                kbd.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(ptr) = self.pointer.take() {
                ptr.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if matches!(event.keysym, Keysym::Escape | Keysym::q) {
            self.exit = true;
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.window.wl_surface() {
                continue;
            }

            if let PointerEventKind::Press { button: 0x110, .. } = event.kind {
                let x = event.position.0 as i32;
                let y = event.position.1 as i32;
                self.toggle_subsurface(x, y, qh);
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl BackgroundEffectHandler for App {
    fn background_effect_state(&mut self) -> &mut BackgroundEffectState {
        &mut self.bg_effect
    }

    fn update_capabilities(&mut self) {
        println!(
            "Background effect capabilities: {:?}",
            self.bg_effect.capabilities()
        );
    }
}

impl Dispatch<wl_region::WlRegion, ()> for App {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_subcompositor!(App);
delegate_background_effect!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
