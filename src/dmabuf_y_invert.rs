//! This example creates a window using dmabuf buffers.
//! Pressing spacebar toggles the y_invert dmabuf flag.
//! Pressing 1..=8 selects a wl_surface::set_buffer_transform value.
//! For every combination of y_invert and buffer_transform the buffer contents
//! are pre-transformed so that the surface should appear identical if the
//! compositor handles everything correctly.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_dmabuf, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_xdg_shell, delegate_xdg_window,
    dmabuf::{DmabufFeedback, DmabufHandler, DmabufState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, QueueHandle,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};

// --- DRM dumb buffer helpers ---

// DRM_FORMAT_XRGB8888
const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
// DRM_FORMAT_MOD_LINEAR
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

// DRM ioctl numbers (Linux):
// _IOWR('d', 0x2d, 12) for PRIME_HANDLE_TO_FD
const DRM_IOCTL_PRIME_HANDLE_TO_FD: libc::c_ulong = 0xC00C642D;
// _IOWR('d', 0xB2, 32) for MODE_CREATE_DUMB
const DRM_IOCTL_MODE_CREATE_DUMB: libc::c_ulong = 0xC02064B2;
// _IOWR('d', 0xB3, 16) for MODE_MAP_DUMB
const DRM_IOCTL_MODE_MAP_DUMB: libc::c_ulong = 0xC01064B3;
// _IOWR('d', 0xB4, 4) for MODE_DESTROY_DUMB
const DRM_IOCTL_MODE_DESTROY_DUMB: libc::c_ulong = 0xC00464B4;

#[repr(C)]
#[derive(Default)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct DrmModeDestroyDumb {
    handle: u32,
}

/// A DRM dumb buffer with its dmabuf fd.
struct DumbBuffer {
    dmabuf_fd: OwnedFd,
    stride: u32,
}

impl DumbBuffer {
    fn create(drm_fd: &OwnedFd, width: u32, height: u32, pixel_data: &[u8]) -> Self {
        // Create a dumb buffer.
        let mut create = DrmModeCreateDumb {
            height,
            width,
            bpp: 32,
            ..Default::default()
        };
        let ret =
            unsafe { libc::ioctl(drm_fd.as_raw_fd(), DRM_IOCTL_MODE_CREATE_DUMB, &mut create) };
        assert!(
            ret == 0,
            "DRM_IOCTL_MODE_CREATE_DUMB failed: {}",
            std::io::Error::last_os_error()
        );

        // Map the dumb buffer for CPU access.
        let mut map = DrmModeMapDumb {
            handle: create.handle,
            ..Default::default()
        };
        let ret = unsafe { libc::ioctl(drm_fd.as_raw_fd(), DRM_IOCTL_MODE_MAP_DUMB, &mut map) };
        assert!(
            ret == 0,
            "DRM_IOCTL_MODE_MAP_DUMB failed: {}",
            std::io::Error::last_os_error()
        );

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                create.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                drm_fd.as_raw_fd(),
                map.offset as libc::off_t,
            )
        };
        assert!(
            ptr != libc::MAP_FAILED,
            "mmap failed: {}",
            std::io::Error::last_os_error()
        );

        // Copy pixel data into the buffer, respecting stride.
        let mapped =
            unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, create.size as usize) };
        let src_stride = width as usize * 4;
        let dst_stride = create.pitch as usize;
        for y in 0..height as usize {
            let src_offset = y * src_stride;
            let dst_offset = y * dst_stride;
            mapped[dst_offset..dst_offset + src_stride]
                .copy_from_slice(&pixel_data[src_offset..src_offset + src_stride]);
        }

        unsafe {
            libc::munmap(ptr, create.size as usize);
        }

        // Export as dmabuf fd via PRIME.
        let mut prime = DrmPrimeHandle {
            handle: create.handle,
            flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            ..Default::default()
        };
        let ret =
            unsafe { libc::ioctl(drm_fd.as_raw_fd(), DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut prime) };
        assert!(
            ret == 0,
            "DRM_IOCTL_PRIME_HANDLE_TO_FD failed: {}",
            std::io::Error::last_os_error()
        );

        let dmabuf_fd = unsafe { OwnedFd::from_raw_fd(prime.fd) };

        DumbBuffer {
            dmabuf_fd,
            stride: create.pitch,
        }
    }
}

fn open_drm_device() -> Option<OwnedFd> {
    let candidates = (128..192)
        .map(|i| format!("/dev/dri/renderD{}", i))
        .chain((0..16).map(|i| format!("/dev/dri/card{}", i)));

    for path_str in candidates {
        let path = std::path::Path::new(&path_str);
        if !path.exists() {
            continue;
        }
        let Ok(file) = std::fs::File::options().read(true).write(true).open(path) else {
            continue;
        };
        let fd: OwnedFd = file.into();

        // Test if dumb buffer creation works on this device.
        let mut create = DrmModeCreateDumb {
            height: 1,
            width: 1,
            bpp: 32,
            ..Default::default()
        };
        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_MODE_CREATE_DUMB, &mut create) };
        if ret == 0 {
            // Clean up the test buffer.
            let mut destroy = DrmModeDestroyDumb {
                handle: create.handle,
            };
            unsafe {
                libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_MODE_DESTROY_DUMB, &mut destroy);
            }
            println!("Using DRM device: {}", path_str);
            return Some(fd);
        }
    }
    None
}

/// The intended color at surface coordinates (sx, sy).
fn target_color(sx: u32, sy: u32, surface_width: u32, surface_height: u32) -> u32 {
    let a = 0xFFu32;
    let r = u32::min(
        ((surface_width - sx) * 0xFF) / surface_width,
        ((surface_height - sy) * 0xFF) / surface_height,
    );
    let g = u32::min(
        (sx * 0xFF) / surface_width,
        ((surface_height - sy) * 0xFF) / surface_height,
    );
    let b = u32::min(
        ((surface_width - sx) * 0xFF) / surface_width,
        (sy * 0xFF) / surface_height,
    );
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Buffer dimensions for the given surface dimensions and buffer_transform.
/// 90/270 transforms swap the dimensions.
fn buffer_dims(sw: u32, sh: u32, transform: wl_output::Transform) -> (u32, u32) {
    use wl_output::Transform::*;
    match transform {
        Normal | _180 | Flipped | Flipped180 => (sw, sh),
        _90 | _270 | Flipped90 | Flipped270 => (sh, sw),
        _ => (sw, sh),
    }
}

/// Map surface-space (sx, sy) to buffer-space (bx, by) for the given transform.
/// The compositor applies the inverse transform when using the buffer, so writing
/// `target_color(sx, sy)` at `map(sx, sy)` produces the desired surface image.
fn map_surface_to_buffer(
    sx: u32,
    sy: u32,
    sw: u32,
    sh: u32,
    transform: wl_output::Transform,
) -> (u32, u32) {
    use wl_output::Transform::*;
    match transform {
        Normal => (sx, sy),
        _90 => (sy, sw - 1 - sx),
        _180 => (sw - 1 - sx, sh - 1 - sy),
        _270 => (sh - 1 - sy, sx),
        Flipped => (sw - 1 - sx, sy),
        Flipped90 => (sy, sx),
        Flipped180 => (sx, sh - 1 - sy),
        Flipped270 => (sh - 1 - sy, sw - 1 - sx),
        _ => (sx, sy),
    }
}

/// Render buffer pixel data that, when the compositor applies the inverse of
/// `transform` (and a vertical flip if `y_inverted`), results in the original
/// gradient at surface coordinates.
fn draw_transformed_gradient(
    surface_width: u32,
    surface_height: u32,
    transform: wl_output::Transform,
    y_inverted: bool,
) -> (u32, u32, Vec<u8>) {
    let (bw, bh) = buffer_dims(surface_width, surface_height, transform);
    let stride = bw * 4;
    let mut data = vec![0u8; (stride * bh) as usize];
    for sy in 0..surface_height {
        for sx in 0..surface_width {
            let color = target_color(sx, sy, surface_width, surface_height);
            let (bx, by) = map_surface_to_buffer(sx, sy, surface_width, surface_height, transform);
            let by = if y_inverted { bh - 1 - by } else { by };
            let offset = (by * stride + bx * 4) as usize;
            data[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
        }
    }
    (bw, bh, data)
}

const TRANSFORMS: [wl_output::Transform; 8] = [
    wl_output::Transform::Normal,
    wl_output::Transform::_90,
    wl_output::Transform::_180,
    wl_output::Transform::_270,
    wl_output::Transform::Flipped,
    wl_output::Transform::Flipped90,
    wl_output::Transform::Flipped180,
    wl_output::Transform::Flipped270,
];

// --- Wayland application ---

fn main() {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<DmabufWindow> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .unwrap();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let dmabuf_state = DmabufState::new(&globals, &qh);

    // Open a DRM device for dumb buffer allocation.
    let drm_fd = open_drm_device().expect("No working DRM device found");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("DMA-BUF Y-Invert Test");
    window.set_min_size(Some((256, 256)));
    window.commit();

    let mut app = DmabufWindow {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        dmabuf_state,

        exit: false,
        first_configure: true,
        width: 256,
        height: 256,
        use_y_invert: false,
        buffer_transform: wl_output::Transform::Normal,
        current_buffer: None,
        current_dumb: None,
        drm_fd,
        window,
        keyboard: None,
        keyboard_focus: false,
        pointer: None,
        loop_handle: event_loop.handle(),
    };

    loop {
        event_loop
            .dispatch(Duration::from_millis(16), &mut app)
            .unwrap();
        if app.exit {
            println!("exiting example");
            break;
        }
    }
}

struct DmabufWindow {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    dmabuf_state: DmabufState,

    exit: bool,
    first_configure: bool,
    width: u32,
    height: u32,
    use_y_invert: bool,
    buffer_transform: wl_output::Transform,
    current_buffer: Option<wl_buffer::WlBuffer>,
    current_dumb: Option<DumbBuffer>,
    drm_fd: OwnedFd,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool,
    pointer: Option<wl_pointer::WlPointer>,
    loop_handle: LoopHandle<'static, DmabufWindow>,
}

impl DmabufWindow {
    fn rebuild_buffer(&mut self, qh: &QueueHandle<Self>) {
        if let Some(buf) = self.current_buffer.take() {
            buf.destroy();
        }
        self.current_dumb = None;

        let (bw, bh, data) = draw_transformed_gradient(
            self.width,
            self.height,
            self.buffer_transform,
            self.use_y_invert,
        );
        let dumb = DumbBuffer::create(&self.drm_fd, bw, bh, &data);

        let params = self
            .dmabuf_state
            .create_params(qh)
            .expect("Failed to create dmabuf params");
        params.add(
            dumb.dmabuf_fd.as_fd(),
            0,
            0,
            dumb.stride,
            DRM_FORMAT_MOD_LINEAR,
        );
        let flags = if self.use_y_invert {
            zwp_linux_buffer_params_v1::Flags::YInvert
        } else {
            zwp_linux_buffer_params_v1::Flags::empty()
        };
        let (wl_buf, _) = params.create_immed(bw as i32, bh as i32, DRM_FORMAT_XRGB8888, flags, qh);

        println!(
            "Rebuilt dmabuf buffer: {}x{} (surface {}x{}), stride={}, y_invert={}, transform={:?}",
            bw, bh, self.width, self.height, dumb.stride, self.use_y_invert, self.buffer_transform
        );

        self.current_buffer = Some(wl_buf);
        self.current_dumb = Some(dumb);
    }

    fn draw(&mut self) {
        let Some(buffer) = self.current_buffer.as_ref() else {
            return;
        };

        let surface = self.window.wl_surface();
        surface.set_buffer_transform(self.buffer_transform);
        surface.attach(Some(buffer), 0, 0);
        let (bw, bh) = buffer_dims(self.width, self.height, self.buffer_transform);
        surface.damage_buffer(0, 0, bw as i32, bh as i32);
        self.window.commit();
    }
}

impl CompositorHandler for DmabufWindow {
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

impl OutputHandler for DmabufWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for DmabufWindow {
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
        println!("Window configured to: {:?}", configure);

        let new_width = configure.new_size.0.map(|v| v.get()).unwrap_or(256);
        let new_height = configure.new_size.1.map(|v| v.get()).unwrap_or(256);

        let size_changed = new_width != self.width || new_height != self.height;
        self.width = new_width;
        self.height = new_height;

        if size_changed || self.first_configure {
            self.first_configure = false;
            self.rebuild_buffer(qh);
            self.draw();
        }
    }
}

impl SeatHandler for DmabufWindow {
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
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|_state, _wl_kbd, _event| {}),
                )
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
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            self.keyboard.take().unwrap().release();
        }

        if capability == Capability::Pointer && self.pointer.is_some() {
            self.pointer.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for DmabufWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        keysyms: &[Keysym],
    ) {
        let _ = keysyms;
        if self.window.wl_surface() == surface {
            self.keyboard_focus = true;
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.window.wl_surface() == surface {
            self.keyboard_focus = false;
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let digit_transform = match event.keysym {
            Keysym::_1 => Some(TRANSFORMS[0]),
            Keysym::_2 => Some(TRANSFORMS[1]),
            Keysym::_3 => Some(TRANSFORMS[2]),
            Keysym::_4 => Some(TRANSFORMS[3]),
            Keysym::_5 => Some(TRANSFORMS[4]),
            Keysym::_6 => Some(TRANSFORMS[5]),
            Keysym::_7 => Some(TRANSFORMS[6]),
            Keysym::_8 => Some(TRANSFORMS[7]),
            _ => None,
        };

        if event.keysym == Keysym::space {
            self.use_y_invert = !self.use_y_invert;
            self.rebuild_buffer(qh);
            self.draw();
        } else if let Some(transform) = digit_transform {
            self.buffer_transform = transform;
            self.rebuild_buffer(qh);
            self.draw();
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

impl PointerHandler for DmabufWindow {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let _ = events;
    }
}

impl DmabufHandler for DmabufWindow {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        println!("Dmabuf feedback: {:?}", feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _buffer: wl_buffer::WlBuffer,
    ) {
        // Not used since we use create_immed.
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        panic!("Failed to create dmabuf buffer");
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
    }
}

delegate_compositor!(DmabufWindow);
delegate_output!(DmabufWindow);

delegate_seat!(DmabufWindow);
delegate_keyboard!(DmabufWindow);
delegate_pointer!(DmabufWindow);

delegate_xdg_shell!(DmabufWindow);
delegate_xdg_window!(DmabufWindow);
delegate_dmabuf!(DmabufWindow);

delegate_registry!(DmabufWindow);

impl ProvidesRegistryState for DmabufWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
