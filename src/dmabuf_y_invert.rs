//! This example creates a window using dmabuf buffers.
//! Pressing spacebar switches between a normal dmabuf and one with the y_invert flag.
//! The rendered contents are vertically flipped versions of each other, so pressing space
//! should not visually change anything if y_invert is handled correctly by the compositor.

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

/// Draw a gradient pattern. When `y_inverted` is true, the rows are flipped so that
/// applying the y_invert flag (vertical flip by the compositor) produces the same image.
fn draw_gradient(width: u32, height: u32, y_inverted: bool) -> Vec<u8> {
    let stride = width * 4;
    let mut data = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        let src_y = if y_inverted { height - 1 - y } else { y };
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            let a = 0xFFu32;
            let r = u32::min(
                ((width - x) * 0xFF) / width,
                ((height - src_y) * 0xFF) / height,
            );
            let g = u32::min((x * 0xFF) / width, ((height - src_y) * 0xFF) / height);
            let b = u32::min(((width - x) * 0xFF) / width, (src_y * 0xFF) / height);
            let color = (a << 24) | (r << 16) | (g << 8) | b;
            data[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
        }
    }
    data
}

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
        normal_buffer: None,
        y_invert_buffer: None,
        normal_dumb: None,
        y_invert_dumb: None,
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
    normal_buffer: Option<wl_buffer::WlBuffer>,
    y_invert_buffer: Option<wl_buffer::WlBuffer>,
    normal_dumb: Option<DumbBuffer>,
    y_invert_dumb: Option<DumbBuffer>,
    drm_fd: OwnedFd,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool,
    pointer: Option<wl_pointer::WlPointer>,
    loop_handle: LoopHandle<'static, DmabufWindow>,
}

impl DmabufWindow {
    fn create_buffers(&mut self, qh: &QueueHandle<Self>) {
        // Destroy old wl_buffers.
        if let Some(buf) = self.normal_buffer.take() {
            buf.destroy();
        }
        if let Some(buf) = self.y_invert_buffer.take() {
            buf.destroy();
        }
        self.normal_dumb = None;
        self.y_invert_dumb = None;

        let width = self.width;
        let height = self.height;

        // Create normal buffer.
        let normal_data = draw_gradient(width, height, false);
        let normal_dumb = DumbBuffer::create(&self.drm_fd, width, height, &normal_data);

        let params = self
            .dmabuf_state
            .create_params(qh)
            .expect("Failed to create dmabuf params");
        params.add(
            normal_dumb.dmabuf_fd.as_fd(),
            0,
            0,
            normal_dumb.stride,
            DRM_FORMAT_MOD_LINEAR,
        );
        let (normal_wl_buf, _) = params.create_immed(
            width as i32,
            height as i32,
            DRM_FORMAT_XRGB8888,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );

        // Create y-inverted buffer.
        let yi_data = draw_gradient(width, height, true);
        let yi_dumb = DumbBuffer::create(&self.drm_fd, width, height, &yi_data);

        let yi_params = self
            .dmabuf_state
            .create_params(qh)
            .expect("Failed to create dmabuf params");
        yi_params.add(
            yi_dumb.dmabuf_fd.as_fd(),
            0,
            0,
            yi_dumb.stride,
            DRM_FORMAT_MOD_LINEAR,
        );
        let (yinvert_wl_buf, _) = yi_params.create_immed(
            width as i32,
            height as i32,
            DRM_FORMAT_XRGB8888,
            zwp_linux_buffer_params_v1::Flags::YInvert,
            qh,
        );

        println!(
            "Created dmabuf buffers: {}x{}, stride={}, using y_invert={}",
            width, height, normal_dumb.stride, self.use_y_invert
        );

        self.normal_buffer = Some(normal_wl_buf);
        self.y_invert_buffer = Some(yinvert_wl_buf);
        self.normal_dumb = Some(normal_dumb);
        self.y_invert_dumb = Some(yi_dumb);
    }

    fn draw(&mut self) {
        let buffer = if self.use_y_invert {
            self.y_invert_buffer.as_ref()
        } else {
            self.normal_buffer.as_ref()
        };

        let Some(buffer) = buffer else { return };

        let surface = self.window.wl_surface();
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
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
            // Recreate buffers at the new size.
            self.create_buffers(qh);
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
            println!("Set keyboard capability");
            let keyboard = self
                .seat_state
                .get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    self.loop_handle.clone(),
                    Box::new(|_state, _wl_kbd, event| {
                        println!("Repeat: {:?} ", event);
                    }),
                )
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }

        if capability == Capability::Pointer && self.pointer.is_none() {
            println!("Set pointer capability");
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
            println!("Unset keyboard capability");
            self.keyboard.take().unwrap().release();
        }

        if capability == Capability::Pointer && self.pointer.is_some() {
            println!("Unset pointer capability");
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
        if self.window.wl_surface() == surface {
            println!("Keyboard focus on window with pressed syms: {keysyms:?}");
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
            println!("Release keyboard focus on window");
            self.keyboard_focus = false;
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key press: {event:?}");

        if event.keysym == Keysym::space {
            self.use_y_invert = !self.use_y_invert;
            println!(
                "Switched to {} buffer",
                if self.use_y_invert {
                    "y_invert"
                } else {
                    "normal"
                }
            );
            self.draw();
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key repeat: {event:?}");
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        println!("Key release: {event:?}");
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        println!("Update modifiers: {modifiers:?}");
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
        use PointerEventKind::*;
        for event in events {
            if &event.surface != self.window.wl_surface() {
                continue;
            }

            match event.kind {
                Enter { .. } => {
                    println!("Pointer entered @{:?}", event.position);
                }
                Leave { .. } => {
                    println!("Pointer left");
                }
                Motion { .. } => {}
                Press { button, .. } => {
                    println!("Press {:x} @ {:?}", button, event.position);
                }
                Release { button, .. } => {
                    println!("Release {:x} @ {:?}", button, event.position);
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    println!("Scroll H:{horizontal:?}, V:{vertical:?}");
                }
            }
        }
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
