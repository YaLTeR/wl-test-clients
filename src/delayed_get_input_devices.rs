use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface, wl_touch,
};
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use sctk::reexports::protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2, zwp_tablet_pad_dial_v2, zwp_tablet_pad_group_v2, zwp_tablet_pad_ring_v2,
    zwp_tablet_pad_strip_v2, zwp_tablet_pad_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2,
    zwp_tablet_v2,
};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::xdg::XdgShell;
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::{
    delegate_compositor, delegate_output, delegate_registry, delegate_seat, delegate_shm,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};

const WINDOW_W: u32 = 500;
const WINDOW_H: u32 = 400;
const DEVICE_DELAY: Duration = Duration::from_secs(1);

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
    let tablet_manager = globals
        .bind::<zwp_tablet_manager_v2::ZwpTabletManagerV2, _, _>(&qh, 1..=2, ())
        .ok();

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("delayed_get_input_devices");
    window.set_app_id("delayed_get_input_devices");
    window.set_min_size(Some((256, 256)));
    window.commit();

    let pool = SlotPool::new(WINDOW_W as usize * WINDOW_H as usize * 4, &shm)
        .expect("Failed to create pool");

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,

        exit: false,
        first_configure: true,
        surface_rendered: false,
        pool,
        width: WINDOW_W,
        height: WINDOW_H,
        buffer: None,
        window,

        seat: None,
        pointer_seat: None,
        keyboard_seat: None,
        touch_seat: None,
        pointer: None,
        keyboard: None,
        touch: None,
        tablet_manager,
        tablet_seat: None,
        pointer_entered: false,
        keyboard_entered: false,
        active_touches: HashSet::new(),
    };

    println!("=== delayed_get_input_devices test ===");
    println!("The window is rendered before seat input devices are created.");
    println!("During the one-second delay:");
    println!("- place the mouse over the window");
    println!("- focus the window for the keyboard test");
    println!("- hold a touch contact on the window for the touch test");
    println!("- hold a tablet tool over the window for the tablet test");
    println!("Raw pointer, keyboard, touch, and tablet events are printed with sequence warnings.");
    if app.tablet_manager.is_none() {
        println!("zwp_tablet_manager_v2 is not available; skipping the tablet test");
    }

    let mut device_deadline = None;
    let mut devices_requested = false;

    loop {
        let timeout = device_deadline
            .map(|deadline: Instant| deadline.saturating_duration_since(Instant::now()));
        event_loop.dispatch(timeout, &mut app).unwrap();

        if app.exit {
            break;
        }

        if !devices_requested
            && device_deadline.is_none()
            && app.surface_rendered
            && app.has_input_capability()
        {
            device_deadline = Some(Instant::now() + DEVICE_DELAY);
            println!("Surface is rendered; creating seat input devices in one second...");
        }

        if device_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            device_deadline = None;
            devices_requested = app.request_devices(&qh);
        }
    }
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,

    exit: bool,
    first_configure: bool,
    surface_rendered: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    window: Window,

    seat: Option<wl_seat::WlSeat>,
    pointer_seat: Option<wl_seat::WlSeat>,
    keyboard_seat: Option<wl_seat::WlSeat>,
    touch_seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    touch: Option<wl_touch::WlTouch>,
    tablet_manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    tablet_seat: Option<zwp_tablet_seat_v2::ZwpTabletSeatV2>,
    pointer_entered: bool,
    keyboard_entered: bool,
    active_touches: HashSet<i32>,
}

#[derive(Default)]
struct TabletToolData {
    in_proximity: AtomicBool,
    down: AtomicBool,
}

impl TabletToolData {
    fn warn_if_out_of_proximity(&self, event: &str) {
        if !self.in_proximity.load(Ordering::Relaxed) {
            eprintln!("WARNING: received tablet tool {event} without a preceding proximity_in");
        }
    }
}

#[derive(Default)]
struct TabletPadData {
    entered: AtomicBool,
}

impl App {
    fn draw(&mut self) {
        let stride = self.width as i32 * 4;
        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(
                    self.width as i32,
                    self.height as i32,
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
                        self.width as i32,
                        self.height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer");
                *buffer = new_buffer;
                canvas
            }
        };

        canvas.chunks_exact_mut(4).for_each(|pixel| {
            let color: u32 = 0xFF_30_70_B0;
            pixel.copy_from_slice(&color.to_le_bytes());
        });

        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        buffer.attach_to(surface).expect("buffer attach");
        surface.commit();
        self.surface_rendered = true;
    }

    fn has_input_capability(&self) -> bool {
        self.pointer_seat.is_some()
            || self.keyboard_seat.is_some()
            || self.touch_seat.is_some()
            || (self.tablet_manager.is_some() && self.seat.is_some())
    }

    fn request_devices(&mut self, qh: &QueueHandle<Self>) -> bool {
        let mut requested = false;

        if let Some(seat) = &self.pointer_seat {
            println!("calling seat.get_pointer() now");
            self.pointer = Some(seat.get_pointer(qh, ()));
            requested = true;
        }
        if let Some(seat) = &self.keyboard_seat {
            println!("calling seat.get_keyboard() now");
            self.keyboard = Some(seat.get_keyboard(qh, ()));
            requested = true;
        }
        if let Some(seat) = &self.touch_seat {
            println!("calling seat.get_touch() now");
            self.touch = Some(seat.get_touch(qh, ()));
            requested = true;
        }
        if let (Some(manager), Some(seat)) = (&self.tablet_manager, &self.seat) {
            println!("calling tablet_manager.get_tablet_seat() now");
            self.tablet_seat = Some(manager.get_tablet_seat(seat, qh, ()));
            requested = true;
        }

        requested
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
        let size_changed = width != self.width || height != self.height;

        self.width = width;
        self.height = height;

        if self.first_configure || size_changed {
            self.first_configure = false;
            self.buffer = None;
            self.draw();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if self.seat.is_none() {
            self.seat = Some(seat.clone());
        }

        match capability {
            Capability::Pointer if self.pointer_seat.is_none() => {
                println!("Pointer capability announced; delaying get_pointer()");
                self.pointer_seat = Some(seat);
            }
            Capability::Keyboard if self.keyboard_seat.is_none() => {
                println!("Keyboard capability announced; delaying get_keyboard()");
                self.keyboard_seat = Some(seat);
            }
            Capability::Touch if self.touch_seat.is_none() => {
                println!("Touch capability announced; delaying get_touch()");
                self.touch_seat = Some(seat);
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Pointer if self.pointer_seat.as_ref() == Some(&seat) => {
                self.pointer_seat = None;
                if let Some(pointer) = self.pointer.take() {
                    pointer.release();
                }
                self.pointer_entered = false;
            }
            Capability::Keyboard if self.keyboard_seat.as_ref() == Some(&seat) => {
                self.keyboard_seat = None;
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
                self.keyboard_entered = false;
            }
            Capability::Touch if self.touch_seat.as_ref() == Some(&seat) => {
                self.touch_seat = None;
                if let Some(touch) = self.touch.take() {
                    touch.release();
                }
                self.active_touches.clear();
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
            if let Some(tablet_seat) = self.tablet_seat.take() {
                tablet_seat.destroy();
            }
        }
        if self.pointer_seat.as_ref() == Some(&seat) {
            self.pointer_seat = None;
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            self.pointer_entered = false;
        }
        if self.keyboard_seat.as_ref() == Some(&seat) {
            self.keyboard_seat = None;
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
            self.keyboard_entered = false;
        }
        if self.touch_seat.as_ref() == Some(&seat) {
            self.touch_seat = None;
            if let Some(touch) = self.touch.take() {
                touch.release();
            }
            self.active_touches.clear();
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                app.pointer_entered = true;
                println!(
                    "enter: serial={serial}, position=({surface_x:.1}, {surface_y:.1}), own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            wl_pointer::Event::Leave { serial, surface } => {
                app.pointer_entered = false;
                println!(
                    "leave: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                println!("motion: time={time}, position=({surface_x:.1}, {surface_y:.1})");
                if !app.pointer_entered {
                    eprintln!("WARNING: received motion without a preceding pointer enter");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                app.keyboard_entered = true;
                println!(
                    "keyboard enter: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            wl_keyboard::Event::Leave { serial, surface } => {
                app.keyboard_entered = false;
                println!(
                    "keyboard leave: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            wl_keyboard::Event::Key {
                serial,
                time,
                key,
                state,
            } => {
                println!("keyboard key: serial={serial}, time={time}, key={key}, state={state:?}");
                if !app.keyboard_entered {
                    eprintln!("WARNING: received keyboard key without a preceding keyboard enter");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for App {
    fn event(
        app: &mut Self,
        _: &wl_touch::WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                serial,
                time,
                surface,
                id,
                x,
                y,
            } => {
                println!(
                    "touch down: serial={serial}, time={time}, id={id}, position=({x:.1}, {y:.1}), own_surface={}",
                    &surface == app.window.wl_surface()
                );
                if !app.active_touches.insert(id) {
                    eprintln!("WARNING: received duplicate touch down for id {id}");
                }
            }
            wl_touch::Event::Up { serial, time, id } => {
                println!("touch up: serial={serial}, time={time}, id={id}");
                if !app.active_touches.remove(&id) {
                    eprintln!("WARNING: received touch up without a preceding down for id {id}");
                }
            }
            wl_touch::Event::Motion { time, id, x, y } => {
                println!("touch motion: time={time}, id={id}, position=({x:.1}, {y:.1})");
                if !app.active_touches.contains(&id) {
                    eprintln!(
                        "WARNING: received touch motion without a preceding down for id {id}"
                    );
                }
            }
            wl_touch::Event::Shape { id, major, minor } => {
                println!("touch shape: id={id}, major={major:.1}, minor={minor:.1}");
                if !app.active_touches.contains(&id) {
                    eprintln!("WARNING: received touch shape without a preceding down for id {id}");
                }
            }
            wl_touch::Event::Orientation { id, orientation } => {
                println!("touch orientation: id={id}, orientation={orientation:.1}");
                if !app.active_touches.contains(&id) {
                    eprintln!(
                        "WARNING: received touch orientation without a preceding down for id {id}"
                    );
                }
            }
            wl_touch::Event::Cancel => {
                println!("touch cancel");
                app.active_touches.clear();
            }
            wl_touch::Event::Frame => println!("touch frame"),
            _ => {}
        }
    }
}

impl Dispatch<zwp_tablet_manager_v2::ZwpTabletManagerV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_manager_v2::ZwpTabletManagerV2,
        _: zwp_tablet_manager_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_seat_v2::ZwpTabletSeatV2,
        event: zwp_tablet_seat_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_seat_v2::Event::TabletAdded { .. } => println!("tablet added"),
            zwp_tablet_seat_v2::Event::ToolAdded { .. } => println!("tablet tool added"),
            zwp_tablet_seat_v2::Event::PadAdded { .. } => println!("tablet pad added"),
            _ => {}
        }
    }

    sctk::reexports::client::event_created_child!(App, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, TabletToolData::default()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (zwp_tablet_pad_v2::ZwpTabletPadV2, TabletPadData::default())
    ]);
}

impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, TabletToolData> for App {
    fn event(
        app: &mut Self,
        tool: &zwp_tablet_tool_v2::ZwpTabletToolV2,
        event: zwp_tablet_tool_v2::Event,
        data: &TabletToolData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_tool_v2::Event::ProximityIn {
                serial, surface, ..
            } => {
                if data.in_proximity.swap(true, Ordering::Relaxed) {
                    eprintln!("WARNING: received duplicate tablet tool proximity_in");
                }
                println!(
                    "tablet proximity_in: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            zwp_tablet_tool_v2::Event::ProximityOut => {
                if !data.in_proximity.swap(false, Ordering::Relaxed) {
                    eprintln!("WARNING: received tablet tool proximity_out without proximity_in");
                }
                if data.down.swap(false, Ordering::Relaxed) {
                    eprintln!("WARNING: received tablet tool proximity_out without a preceding up");
                }
                println!("tablet proximity_out");
            }
            zwp_tablet_tool_v2::Event::Down { serial } => {
                data.warn_if_out_of_proximity("down");
                if data.down.swap(true, Ordering::Relaxed) {
                    eprintln!("WARNING: received duplicate tablet tool down");
                }
                println!("tablet down: serial={serial}");
            }
            zwp_tablet_tool_v2::Event::Up => {
                data.warn_if_out_of_proximity("up");
                if !data.down.swap(false, Ordering::Relaxed) {
                    eprintln!("WARNING: received tablet tool up without a preceding down");
                }
                println!("tablet up");
            }
            zwp_tablet_tool_v2::Event::Motion { x, y } => {
                data.warn_if_out_of_proximity("motion");
                println!("tablet motion: position=({x:.1}, {y:.1})");
            }
            zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                data.warn_if_out_of_proximity("pressure");
                println!("tablet pressure: {pressure}");
            }
            zwp_tablet_tool_v2::Event::Distance { distance } => {
                data.warn_if_out_of_proximity("distance");
                println!("tablet distance: {distance}");
            }
            zwp_tablet_tool_v2::Event::Tilt { tilt_x, tilt_y } => {
                data.warn_if_out_of_proximity("tilt");
                println!("tablet tilt: ({tilt_x:.1}, {tilt_y:.1})");
            }
            zwp_tablet_tool_v2::Event::Rotation { degrees } => {
                data.warn_if_out_of_proximity("rotation");
                println!("tablet rotation: {degrees:.1}");
            }
            zwp_tablet_tool_v2::Event::Slider { position } => {
                data.warn_if_out_of_proximity("slider");
                println!("tablet slider: {position}");
            }
            zwp_tablet_tool_v2::Event::Wheel { degrees, clicks } => {
                data.warn_if_out_of_proximity("wheel");
                println!("tablet wheel: degrees={degrees:.1}, clicks={clicks}");
            }
            zwp_tablet_tool_v2::Event::Button {
                serial,
                button,
                state,
            } => {
                data.warn_if_out_of_proximity("button");
                println!("tablet button: serial={serial}, button={button}, state={state:?}");
            }
            zwp_tablet_tool_v2::Event::Frame { time } => {
                println!("tablet frame: time={time}");
            }
            zwp_tablet_tool_v2::Event::Removed => {
                if data.in_proximity.load(Ordering::Relaxed) {
                    eprintln!("WARNING: tablet tool removed without proximity_out");
                }
                if data.down.load(Ordering::Relaxed) {
                    eprintln!("WARNING: tablet tool removed without up");
                }
                println!("tablet tool removed");
                tool.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for App {
    fn event(
        _: &mut Self,
        tablet: &zwp_tablet_v2::ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_tablet_v2::Event::Removed = event {
            println!("tablet removed");
            tablet.destroy();
        }
    }
}

impl Dispatch<zwp_tablet_pad_v2::ZwpTabletPadV2, TabletPadData> for App {
    fn event(
        app: &mut Self,
        pad: &zwp_tablet_pad_v2::ZwpTabletPadV2,
        event: zwp_tablet_pad_v2::Event,
        data: &TabletPadData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_tablet_pad_v2::Event::Enter {
                serial, surface, ..
            } => {
                data.entered.store(true, Ordering::Relaxed);
                println!(
                    "tablet pad enter: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            zwp_tablet_pad_v2::Event::Leave { serial, surface } => {
                data.entered.store(false, Ordering::Relaxed);
                println!(
                    "tablet pad leave: serial={serial}, own_surface={}",
                    &surface == app.window.wl_surface()
                );
            }
            zwp_tablet_pad_v2::Event::Button {
                time,
                button,
                state,
            } => {
                println!("tablet pad button: time={time}, button={button}, state={state:?}");
                if !data.entered.load(Ordering::Relaxed) {
                    eprintln!("WARNING: received tablet pad button without a preceding enter");
                }
            }
            zwp_tablet_pad_v2::Event::Removed => {
                data.entered.store(false, Ordering::Relaxed);
                println!("tablet pad removed");
                pad.destroy();
            }
            _ => {}
        }
    }

    sctk::reexports::client::event_created_child!(App, zwp_tablet_pad_v2::ZwpTabletPadV2, [
        zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ())
    ]);
}

impl Dispatch<zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
        _: zwp_tablet_pad_group_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    sctk::reexports::client::event_created_child!(App, zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, [
        zwp_tablet_pad_group_v2::EVT_RING_OPCODE => (zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()),
        zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => (zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ()),
        zwp_tablet_pad_group_v2::EVT_DIAL_OPCODE => (zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2, ())
    ]);
}

impl Dispatch<zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
        _: zwp_tablet_pad_ring_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
        _: zwp_tablet_pad_strip_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2,
        _: zwp_tablet_pad_dial_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
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

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_registry!(App);
delegate_seat!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState,];
}
