use sctk::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface,
};
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use sctk::reexports::protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1;
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_background_effect, delegate_compositor, delegate_keyboard, delegate_output,
    delegate_pointer, delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

const WINDOW_W: u32 = 400;
const WINDOW_H: u32 = 300;

/// Which half of the surface is covered by blur.
#[derive(Clone, Copy, PartialEq)]
enum BlurHalf {
    Left,
    Right,
}

impl BlurHalf {
    fn toggle(self) -> Self {
        match self {
            BlurHalf::Left => BlurHalf::Right,
            BlurHalf::Right => BlurHalf::Left,
        }
    }

    fn label(self) -> &'static str {
        match self {
            BlurHalf::Left => "left",
            BlurHalf::Right => "right",
        }
    }
}

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
    let bg_effect = BackgroundEffectState::new(&globals, &qh);

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("blur_region_switch_half");
    window.set_min_size(Some((256, 256)));
    window.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let bg_effect_surface = bg_effect
        .get_background_effect(window.wl_surface(), &qh)
        .ok();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        shm,
        bg_effect,

        exit: false,
        first_configure: true,
        pool,
        window_width: WINDOW_W,
        window_height: WINDOW_H,
        window_buffer: None,
        window,
        bg_effect_surface,
        keyboard: None,
        pointer: None,

        blur_half: BlurHalf::Left,
    };

    println!("=== blur_region_switch_half test ===");
    println!("Main surface: semitransparent with blur on the left half.");
    println!("Press Space to switch which half has blur.");
    println!("Only the blur region changes — no buffer damage.");
    println!("Q/Escape to quit.");

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
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    shm: Shm,
    bg_effect: BackgroundEffectState,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    window_width: u32,
    window_height: u32,
    window_buffer: Option<Buffer>,
    window: Window,
    bg_effect_surface: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    blur_half: BlurHalf,
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

    fn update_blur_region(&self, qh: &QueueHandle<Self>) {
        if let Some(ref bg) = self.bg_effect_surface {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            let half_w = self.window_width as i32 / 2;
            match self.blur_half {
                BlurHalf::Left => {
                    region.add(0, 0, half_w, self.window_height as i32);
                }
                BlurHalf::Right => {
                    region.add(
                        half_w,
                        0,
                        self.window_width as i32 - half_w,
                        self.window_height as i32,
                    );
                }
            }
            bg.set_blur_region(Some(&region));
            region.destroy();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
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
        _: &Connection,
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
            self.update_blur_region(qh);
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
        _: &Connection,
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
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(kbd) = self.keyboard.take()
        {
            kbd.release();
        }
        if capability == Capability::Pointer
            && let Some(ptr) = self.pointer.take()
        {
            ptr.release();
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
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Escape | Keysym::q => {
                self.exit = true;
            }
            Keysym::space => {
                self.blur_half = self.blur_half.toggle();
                println!("Blur now on {} half", self.blur_half.label());
                self.update_blur_region(qh);
                // Only commit the surface — no buffer damage, no buffer attach.
                // The compositor must repaint the blur from the region change alone.
                self.window.wl_surface().commit();
            }
            _ => {}
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
        _: &Connection,
        _: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        _events: &[PointerEvent],
    ) {
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
delegate_background_effect!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
