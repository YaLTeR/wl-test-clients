use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_subsurface, wl_surface,
};
use sctk::reexports::client::{Connection, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::subcompositor::SubcompositorState;
use sctk::{
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_subcompositor, delegate_xdg_popup, delegate_xdg_shell,
    delegate_xdg_window, registry_handlers,
};

const WINDOW_W: u32 = 400;
const WINDOW_H: u32 = 300;
const POPUP_W: u32 = 240;
const POPUP_H: u32 = 160;
const SUB_X: i32 = 20;
const SUB_Y: i32 = 20;
const SUB_W: u32 = 200;
const SUB_H: u32 = 120;

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().expect("Failed to initialize the event loop");
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .expect("wl_subcompositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("popup_desync_subsurface_frame_callbacks");
    window.set_min_size(Some((WINDOW_W, WINDOW_H)));
    window.commit();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        subcompositor,
        xdg_shell,
        shm,
        exit: false,
        first_configure: true,
        pool,
        window_width: WINDOW_W,
        window_height: WINDOW_H,
        window_buffer: None,
        window,
        keyboard: None,
        pointer: None,
        popup_position: (WINDOW_W as i32 / 2, WINDOW_H as i32 / 2),
        popup: None,
    };

    println!("=== popup_desync_subsurface_frame_callbacks test ===");
    println!("Left click or Space: open/close the popup.");
    println!("Only the popup's desynchronized subsurface animates and commits after setup.");
    println!("Q/Escape: quit.");

    loop {
        event_loop.dispatch(None, &mut app).unwrap();
        if app.exit {
            break;
        }
    }
}

struct AnimatedSubsurface {
    subsurface: wl_subsurface::WlSubsurface,
    surface: wl_surface::WlSurface,
    buffers: [Option<Buffer>; 2],
    current_buffer: usize,
    frame: u32,
}

struct PopupState {
    popup: Popup,
    buffer: Option<Buffer>,
    subsurface: Option<AnimatedSubsurface>,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    xdg_shell: XdgShell,
    shm: Shm,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    window_width: u32,
    window_height: u32,
    window_buffer: Option<Buffer>,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    popup_position: (i32, i32),
    popup: Option<PopupState>,
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
                .expect("create window buffer")
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
                    .expect("create window buffer");
                *buffer = new_buffer;
                canvas
            }
        };

        for (index, pixel) in canvas.chunks_exact_mut(4).enumerate() {
            let x = index as u32 % width;
            let y = index as u32 / width;
            let color: u32 = if (x / 32 + y / 32) % 2 == 0 {
                0xff_28_28_28
            } else {
                0xff_38_38_38
            };
            pixel.copy_from_slice(&color.to_le_bytes());
        }

        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        buffer.attach_to(surface).expect("attach window buffer");
        self.window.commit();
    }

    fn toggle_popup(&mut self, qh: &QueueHandle<Self>) {
        if self.popup.is_some() {
            self.close_popup();
        } else {
            self.open_popup(qh);
        }
    }

    fn open_popup(&mut self, qh: &QueueHandle<Self>) {
        let positioner = XdgPositioner::new(&self.xdg_shell).expect("create positioner");
        positioner.set_size(POPUP_W as i32, POPUP_H as i32);
        positioner.set_anchor_rect(self.popup_position.0, self.popup_position.1, 1, 1);
        positioner.set_anchor(Anchor::Bottom);
        positioner.set_gravity(Gravity::Bottom);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY
                | ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY,
        );

        let popup = Popup::new(
            self.window.xdg_surface(),
            &positioner,
            qh,
            &self.compositor,
            &self.xdg_shell,
        )
        .expect("create popup");
        popup
            .xdg_surface()
            .set_window_geometry(0, 0, POPUP_W as i32, POPUP_H as i32);

        self.popup = Some(PopupState {
            popup,
            buffer: None,
            subsurface: None,
        });
        println!("popup opened; waiting for configure");
    }

    fn configure_popup(&mut self, qh: &QueueHandle<Self>) {
        self.draw_popup_once();
        self.create_animated_subsurface(qh);

        // Apply the new subsurface's position. After this setup commit, the popup
        // parent is never committed again; animation commits only the desync child.
        self.popup.as_ref().unwrap().popup.wl_surface().commit();
        println!("popup configured; animating only the desync subsurface");
    }

    fn draw_popup_once(&mut self) {
        let state = self.popup.as_mut().unwrap();
        let stride = POPUP_W as i32 * 4;
        let buffer = state.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(
                    POPUP_W as i32,
                    POPUP_H as i32,
                    stride,
                    wl_shm::Format::Argb8888,
                )
                .expect("create popup buffer")
                .0
        });
        let canvas = self.pool.canvas(buffer).expect("popup buffer is busy");

        for (index, pixel) in canvas.chunks_exact_mut(4).enumerate() {
            let x = index as u32 % POPUP_W;
            let y = index as u32 / POPUP_W;
            let in_child = x >= SUB_X as u32
                && x < SUB_X as u32 + SUB_W
                && y >= SUB_Y as u32
                && y < SUB_Y as u32 + SUB_H;
            let color: u32 = if in_child {
                0xff_10_10_10
            } else {
                0xff_d0_d0_d0
            };
            pixel.copy_from_slice(&color.to_le_bytes());
        }

        let surface = state.popup.wl_surface();
        surface.damage_buffer(0, 0, POPUP_W as i32, POPUP_H as i32);
        buffer.attach_to(surface).expect("attach popup buffer");
        surface.commit();
    }

    fn create_animated_subsurface(&mut self, qh: &QueueHandle<Self>) {
        let parent = self.popup.as_ref().unwrap().popup.wl_surface().clone();
        let (subsurface, surface) = self.subcompositor.create_subsurface(parent, qh);
        subsurface.set_position(SUB_X, SUB_Y);
        subsurface.set_desync();

        self.popup.as_mut().unwrap().subsurface = Some(AnimatedSubsurface {
            subsurface,
            surface,
            buffers: [None, None],
            current_buffer: 0,
            frame: 0,
        });
        self.submit_subsurface_frame(qh);
    }

    fn submit_subsurface_frame(&mut self, qh: &QueueHandle<Self>) {
        let child = self
            .popup
            .as_mut()
            .and_then(|popup| popup.subsurface.as_mut())
            .expect("animated subsurface missing");
        let buffer_index = child.current_buffer;
        let stride = SUB_W as i32 * 4;
        let buffer = child.buffers[buffer_index].get_or_insert_with(|| {
            self.pool
                .create_buffer(SUB_W as i32, SUB_H as i32, stride, wl_shm::Format::Argb8888)
                .expect("create subsurface buffer")
                .0
        });
        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (new_buffer, canvas) = self
                    .pool
                    .create_buffer(SUB_W as i32, SUB_H as i32, stride, wl_shm::Format::Argb8888)
                    .expect("create subsurface buffer");
                *buffer = new_buffer;
                canvas
            }
        };

        let stripe_x = child.frame % SUB_W;
        for (index, pixel) in canvas.chunks_exact_mut(4).enumerate() {
            let x = index as u32 % SUB_W;
            let y = index as u32 / SUB_W;
            let stripe_distance = (x + SUB_W - stripe_x) % SUB_W;
            let color: u32 = if stripe_distance < 32 {
                0xff_ff_b0_20
            } else if (x / 20 + y / 20) % 2 == 0 {
                0xff_20_50_a0
            } else {
                0xff_30_70_d0
            };
            pixel.copy_from_slice(&color.to_le_bytes());
        }

        buffer
            .attach_to(&child.surface)
            .expect("attach subsurface buffer");
        child
            .surface
            .damage_buffer(0, 0, SUB_W as i32, SUB_H as i32);
        child.surface.frame(qh, child.surface.clone());
        child.surface.commit();

        child.current_buffer ^= 1;
        child.frame = child.frame.wrapping_add(3);
    }

    fn close_popup(&mut self) {
        if let Some(mut state) = self.popup.take() {
            if let Some(child) = state.subsurface.take() {
                child.subsurface.destroy();
                child.surface.destroy();
            }
            println!("popup closed");
        }
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

    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        let is_animated_subsurface = self
            .popup
            .as_ref()
            .and_then(|popup| popup.subsurface.as_ref())
            .is_some_and(|child| &child.surface == surface);
        if is_animated_subsurface {
            self.submit_subsurface_frame(qh);
        }
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
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let width = configure
            .new_size
            .0
            .map(|value| value.get())
            .unwrap_or(WINDOW_W);
        let height = configure
            .new_size
            .1
            .map(|value| value.get())
            .unwrap_or(WINDOW_H);
        let size_changed = width != self.window_width || height != self.window_height;
        self.window_width = width;
        self.window_height = height;

        if self.first_configure || size_changed {
            self.first_configure = false;
            self.window_buffer = None;
            self.draw_window();
        }
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        popup: &Popup,
        _: PopupConfigure,
    ) {
        let needs_setup = self
            .popup
            .as_ref()
            .is_some_and(|state| &state.popup == popup && state.subsurface.is_none());
        if needs_setup {
            self.configure_popup(qh);
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self
            .popup
            .as_ref()
            .is_some_and(|state| &state.popup == popup)
        {
            self.close_popup();
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
            self.keyboard = Some(
                self.seat_state
                    .get_keyboard(qh, &seat, None)
                    .expect("create keyboard"),
            );
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = Some(
                self.seat_state
                    .get_pointer(qh, &seat)
                    .expect("create pointer"),
            );
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
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
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
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
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
            Keysym::space => self.toggle_popup(qh),
            Keysym::q | Keysym::Escape => self.exit = true,
            _ => {}
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
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
            if &event.surface != self.window.wl_surface() {
                continue;
            }
            self.popup_position = (event.position.0 as i32, event.position.1 as i32);
            if let PointerEventKind::Press { button: 0x110, .. } = event.kind {
                self.toggle_popup(qh);
            }
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
delegate_seat!(App);
delegate_keyboard!(App);
delegate_pointer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_xdg_popup!(App);
delegate_subcompositor!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
