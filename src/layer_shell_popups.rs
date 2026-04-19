use sctk::background_effect::{BackgroundEffectHandler, BackgroundEffectState};
use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_subsurface, wl_surface,
};
use sctk::reexports::client::{Connection, Dispatch, QueueHandle};
use sctk::reexports::protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1;
use sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use sctk::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowHandler};
use sctk::shell::xdg::{XdgPositioner, XdgShell};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::subcompositor::SubcompositorState;
use sctk::{
    delegate_background_effect, delegate_compositor, delegate_keyboard, delegate_layer,
    delegate_output, delegate_pointer, delegate_registry, delegate_seat, delegate_shm,
    delegate_subcompositor, delegate_xdg_popup, delegate_xdg_shell, delegate_xdg_window,
    registry_handlers,
};

const LAYER_W: u32 = 400;
const LAYER_H: u32 = 300;

const DEFAULT_POPUP_SURFACE_W: u32 = 260;
const DEFAULT_POPUP_SURFACE_H: u32 = 200;
const DEFAULT_GEOMETRY_INSET: u32 = 30;

const CHILD_POPUP_W: u32 = 120;
const CHILD_POPUP_H: u32 = 160;

const SUB_W: u32 = 200;
const SUB_H: u32 = 200;

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Background => "Background",
        Layer::Bottom => "Bottom",
        Layer::Top => "Top",
        Layer::Overlay => "Overlay",
        _ => "Unknown",
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
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .expect("wl_subcompositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell is not available");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg shell is not available");
    let shm = Shm::bind(&globals, &qh).expect("wl shm is not available.");
    let bg_effect = BackgroundEffectState::new(&globals, &qh);

    let current_layer = Layer::Top;

    let surface = compositor.create_surface(&qh);
    let layer_surface = layer_shell.create_layer_surface(
        &qh,
        surface,
        current_layer,
        Some("layer_shell_popups"),
        None,
    );
    // Centered on screen: set size but don't anchor to any edges.
    layer_surface.set_size(LAYER_W, LAYER_H);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    // No exclusive zone.
    layer_surface.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("Failed to create pool");

    let layer_bg_effect = bg_effect
        .get_background_effect(layer_surface.wl_surface(), &qh)
        .ok();

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        subcompositor,
        layer_shell,
        xdg_shell,
        shm,
        bg_effect,

        exit: false,
        first_configure: true,
        pool,
        width: LAYER_W,
        height: LAYER_H,
        buffer: None,
        layer_surface,
        layer_bg_effect,
        current_layer,
        keyboard: None,
        pointer: None,

        parent_popup: None,
        child_popup: None,

        click_pos: (0, 0),

        popup_surface_w: DEFAULT_POPUP_SURFACE_W,
        popup_surface_h: DEFAULT_POPUP_SURFACE_H,
        geometry_inset: DEFAULT_GEOMETRY_INSET,
        popup_buffer_scale: 1,

        subsurface: None,
    };

    println!("=== layer_shell_popups test ===");
    println!("Layer-shell surface, centered, no exclusive zone.");
    println!("Left click: toggle popups at click position.");
    println!("Right click: toggle subsurface at click position.");
    println!("Keybinds (while popups are open):");
    println!("  W/S  - increase/decrease popup surface width");
    println!("  E/D  - increase/decrease popup surface height");
    println!("  R/F  - increase/decrease geometry inset (shadow size)");
    println!("  T/G  - increase/decrease popup buffer scale");
    println!("PgUp/PgDn: switch layer up/down (Background < Bottom < Top < Overlay).");
    println!("Q/Escape: quit.");
    println!("Current layer: {}", layer_name(app.current_layer));

    loop {
        event_loop.dispatch(None, &mut app).unwrap();
        if app.exit {
            println!("exiting");
            break;
        }
    }
}

struct PopupState {
    popup: Popup,
    buffer: Option<Buffer>,
    configured: bool,
    bg_effect_surface: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
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
    #[allow(dead_code)]
    layer_shell: LayerShell,
    xdg_shell: XdgShell,
    shm: Shm,
    bg_effect: BackgroundEffectState,

    exit: bool,
    first_configure: bool,
    pool: SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    layer_surface: LayerSurface,
    layer_bg_effect: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    current_layer: Layer,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    parent_popup: Option<PopupState>,
    child_popup: Option<PopupState>,

    click_pos: (i32, i32),

    // Adjustable popup parameters.
    popup_surface_w: u32,
    popup_surface_h: u32,
    geometry_inset: u32,
    popup_buffer_scale: u32,

    subsurface: Option<SubsurfaceState>,
}

impl App {
    fn draw_layer_surface(&mut self) {
        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

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

        self.layer_surface
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.layer_surface.wl_surface())
            .expect("buffer attach");
        self.layer_surface.commit();
    }

    fn update_blur_region(&self, qh: &QueueHandle<Self>) {
        if let Some(ref bg) = self.layer_bg_effect {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            region.add(0, 0, self.width as i32, self.height as i32);
            bg.set_blur_region(Some(&region));
            region.destroy();
        }
    }

    fn layer_up(&self) -> Option<Layer> {
        match self.current_layer {
            Layer::Background => Some(Layer::Bottom),
            Layer::Bottom => Some(Layer::Top),
            Layer::Top => Some(Layer::Overlay),
            Layer::Overlay => None,
            _ => None,
        }
    }

    fn layer_down(&self) -> Option<Layer> {
        match self.current_layer {
            Layer::Background => None,
            Layer::Bottom => Some(Layer::Background),
            Layer::Top => Some(Layer::Bottom),
            Layer::Overlay => Some(Layer::Top),
            _ => None,
        }
    }

    fn set_layer(&mut self, layer: Layer) {
        self.current_layer = layer;
        self.layer_surface.set_layer(layer);
        self.layer_surface.commit();
        println!("Layer: {}", layer_name(layer));
    }

    // --- Popups (left click) ---

    fn open_popups(&mut self, qh: &QueueHandle<Self>) {
        if self.parent_popup.is_some() {
            return;
        }

        let surf_w = self.popup_surface_w;
        let surf_h = self.popup_surface_h;
        let inset = self.geometry_inset;
        let scale = self.popup_buffer_scale;

        // Geometry width/height = surface size minus shadow on each side.
        let geo_w = surf_w.saturating_sub(inset * 2).max(1);
        let geo_h = surf_h.saturating_sub(inset * 2).max(1);

        // --- Parent popup: positioned above the click point ---
        let positioner = XdgPositioner::new(&self.xdg_shell).unwrap();
        positioner.set_size(geo_w as i32, geo_h as i32);
        positioner.set_anchor_rect(self.click_pos.0, self.click_pos.1, 1, 1);
        positioner.set_anchor(Anchor::Top);
        positioner.set_gravity(Gravity::Top);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY
                | ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY,
        );

        // For layer surfaces, we create the popup without a parent xdg_surface,
        // then associate it via layer_surface.get_popup().
        let parent_popup = Popup::from_surface(
            None,
            &positioner,
            qh,
            self.compositor.create_surface(qh),
            &self.xdg_shell,
        )
        .expect("create parent popup");

        self.layer_surface.get_popup(parent_popup.xdg_popup());

        // Set geometry: the content area excluding the shadow border.
        parent_popup.xdg_surface().set_window_geometry(
            inset as i32,
            inset as i32,
            geo_w as i32,
            geo_h as i32,
        );

        // Set buffer scale.
        parent_popup.wl_surface().set_buffer_scale(scale as i32);

        // Create background effect and set blur region matching geometry.
        let bg_surface = self
            .bg_effect
            .get_background_effect(parent_popup.wl_surface(), qh)
            .ok();
        if let Some(ref bg) = bg_surface {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            region.add(inset as i32, inset as i32, geo_w as i32, geo_h as i32);
            bg.set_blur_region(Some(&region));
            region.destroy();
        }

        parent_popup.wl_surface().commit();

        self.parent_popup = Some(PopupState {
            popup: parent_popup,
            buffer: None,
            configured: false,
            bg_effect_surface: bg_surface,
        });

        println!(
            "Opened parent popup: surface={}x{}, geometry={}x{} at inset={}, scale={}",
            surf_w, surf_h, geo_w, geo_h, inset, scale
        );
    }

    fn open_child_popup(&mut self, qh: &QueueHandle<Self>) {
        if self.child_popup.is_some() || self.parent_popup.is_none() {
            return;
        }

        let parent = self.parent_popup.as_ref().unwrap();
        let inset = self.geometry_inset;
        let scale = self.popup_buffer_scale;

        let geo_w = self.popup_surface_w.saturating_sub(inset * 2).max(1);
        let geo_h = self.popup_surface_h.saturating_sub(inset * 2).max(1);

        // Child popup: no geometry, positioned to the right side of the parent.
        let x_overlap = inset as i32 / 2 + 10;
        let positioner = XdgPositioner::new(&self.xdg_shell).unwrap();
        positioner.set_size(CHILD_POPUP_W as i32, CHILD_POPUP_H as i32);
        positioner.set_anchor_rect(geo_w as i32 - x_overlap, geo_h as i32 / 3, 1, 1);
        positioner.set_anchor(Anchor::TopRight);
        positioner.set_gravity(Gravity::TopRight);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY
                | ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY,
        );

        let child_popup = Popup::new(
            parent.popup.xdg_surface(),
            &positioner,
            qh,
            &self.compositor,
            &self.xdg_shell,
        )
        .expect("create child popup");

        // No geometry set on the child — the whole surface is the content.
        child_popup.wl_surface().set_buffer_scale(scale as i32);

        // Set blur region larger than the surface to test compositor clipping.
        let bg_surface = self
            .bg_effect
            .get_background_effect(child_popup.wl_surface(), qh)
            .ok();
        if let Some(ref bg) = bg_surface {
            let region = self.compositor.wl_compositor().create_region(qh, ());
            // Extend 50px outside in every direction.
            region.add(
                -50,
                -50,
                CHILD_POPUP_W as i32 + 100,
                CHILD_POPUP_H as i32 + 100,
            );
            bg.set_blur_region(Some(&region));
            region.destroy();
        }

        self.child_popup = Some(PopupState {
            popup: child_popup,
            buffer: None,
            configured: false,
            bg_effect_surface: bg_surface,
        });

        println!(
            "Opened child popup: surface={}x{}, no geometry, scale={}, blur oversized by 50px",
            CHILD_POPUP_W, CHILD_POPUP_H, scale
        );
    }

    fn close_popups(&mut self) {
        // Child must be destroyed before parent.
        if let Some(mut child) = self.child_popup.take() {
            if let Some(bg) = child.bg_effect_surface.take() {
                bg.destroy();
            }
        }
        if let Some(mut parent) = self.parent_popup.take() {
            if let Some(bg) = parent.bg_effect_surface.take() {
                bg.destroy();
            }
        }
        println!("Closed popups");
    }

    fn draw_parent_popup(&mut self) {
        let Some(ref mut state) = self.parent_popup else {
            return;
        };
        if !state.configured {
            return;
        }

        let surf_w = self.popup_surface_w;
        let surf_h = self.popup_surface_h;
        let inset = self.geometry_inset;
        let scale = self.popup_buffer_scale;
        let buf_w = surf_w * scale;
        let buf_h = surf_h * scale;
        let stride = buf_w as i32 * 4;

        let geo_w = surf_w.saturating_sub(inset * 2).max(1);
        let geo_h = surf_h.saturating_sub(inset * 2).max(1);

        // Set geometry and scale (cheap, no object creation).
        state.popup.xdg_surface().set_window_geometry(
            inset as i32,
            inset as i32,
            geo_w as i32,
            geo_h as i32,
        );
        state.popup.wl_surface().set_buffer_scale(scale as i32);

        // Always create a fresh buffer (size may have changed).
        state.buffer = None;

        let buffer = state.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(buf_w as i32, buf_h as i32, stride, wl_shm::Format::Argb8888)
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(buf_w as i32, buf_h as i32, stride, wl_shm::Format::Argb8888)
                    .expect("create buffer");
                *buffer = second_buffer;
                canvas
            }
        };

        // Draw the popup (premultiplied alpha):
        // - Outside geometry (shadow): semi-transparent dark
        // - Inside geometry (content): semi-transparent light tint (blur visible through it)
        canvas
            .chunks_exact_mut(4)
            .enumerate()
            .for_each(|(index, chunk)| {
                let px = (index as u32) % buf_w;
                let py = (index as u32) / buf_w;
                // Convert buffer coords to surface coords.
                let sx = px / scale;
                let sy = py / scale;

                let in_geometry =
                    sx >= inset && sx < inset + geo_w && sy >= inset && sy < inset + geo_h;

                // Premultiplied alpha: channel values must be <= alpha.
                let color: u32 = if in_geometry {
                    // Content area: ~19% white. A=0x30, RGB=0x30.
                    0x30_30_30_30
                } else {
                    // Shadow area: ~25% black. A=0x40, RGB=0x00.
                    0x40_00_00_00
                };
                chunk.copy_from_slice(&color.to_le_bytes());
            });

        let popup_surface = state.popup.wl_surface();
        popup_surface.damage_buffer(0, 0, buf_w as i32, buf_h as i32);
        buffer.attach_to(popup_surface).expect("buffer attach");
        popup_surface.commit();
    }

    fn draw_child_popup(&mut self) {
        let Some(ref mut state) = self.child_popup else {
            return;
        };
        if !state.configured {
            return;
        }

        let scale = self.popup_buffer_scale;
        let buf_w = CHILD_POPUP_W * scale;
        let buf_h = CHILD_POPUP_H * scale;
        let stride = buf_w as i32 * 4;

        // Set scale (cheap, no object creation).
        state.popup.wl_surface().set_buffer_scale(scale as i32);

        // Always create a fresh buffer (scale may have changed).
        state.buffer = None;

        let buffer = state.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(buf_w as i32, buf_h as i32, stride, wl_shm::Format::Argb8888)
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (second_buffer, canvas) = self
                    .pool
                    .create_buffer(buf_w as i32, buf_h as i32, stride, wl_shm::Format::Argb8888)
                    .expect("create buffer");
                *buffer = second_buffer;
                canvas
            }
        };

        // Semi-transparent purple tint (premultiplied alpha).
        // A=0x30, R=0x18, G=0x12, B=0x30.
        canvas.chunks_exact_mut(4).for_each(|chunk| {
            let color: u32 = 0x30_18_12_30;
            chunk.copy_from_slice(&color.to_le_bytes());
        });

        let popup_surface = state.popup.wl_surface();
        popup_surface.damage_buffer(0, 0, buf_w as i32, buf_h as i32);
        buffer.attach_to(popup_surface).expect("buffer attach");
        popup_surface.commit();
    }

    /// Update double-buffered blur region state and redraw popups.
    /// Called only when parameters change via keybinds.
    fn update_popups(&mut self, qh: &QueueHandle<Self>) {
        if self.parent_popup.is_none() {
            return;
        }

        // Update parent blur region.
        if let Some(ref state) = self.parent_popup {
            if let Some(ref bg) = state.bg_effect_surface {
                let inset = self.geometry_inset;
                let geo_w = self.popup_surface_w.saturating_sub(inset * 2).max(1);
                let geo_h = self.popup_surface_h.saturating_sub(inset * 2).max(1);
                let region = self.compositor.wl_compositor().create_region(qh, ());
                region.add(inset as i32, inset as i32, geo_w as i32, geo_h as i32);
                bg.set_blur_region(Some(&region));
                region.destroy();
            }
        }

        // Update child blur region.
        if let Some(ref state) = self.child_popup {
            if let Some(ref bg) = state.bg_effect_surface {
                let region = self.compositor.wl_compositor().create_region(qh, ());
                region.add(
                    -50,
                    -50,
                    CHILD_POPUP_W as i32 + 100,
                    CHILD_POPUP_H as i32 + 100,
                );
                bg.set_blur_region(Some(&region));
                region.destroy();
            }
        }

        // Redraw and commit (picks up geometry/scale/blur changes).
        self.draw_parent_popup();
        self.draw_child_popup();
    }

    // --- Subsurface (right click) ---

    fn create_subsurface(&mut self, click_x: i32, click_y: i32, qh: &QueueHandle<Self>) {
        if self.subsurface.is_some() {
            return;
        }

        let (subsurface, surface) = self
            .subcompositor
            .create_subsurface(self.layer_surface.wl_surface().clone(), qh);

        // Center the subsurface at the click position.
        let x = click_x - (SUB_W as i32) / 2;
        let y = click_y - (SUB_H as i32) / 2;
        subsurface.set_position(x, y);
        subsurface.set_desync();

        // Set blur on the subsurface.
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
        self.layer_surface.wl_surface().commit();

        println!("Created subsurface at ({}, {})", x, y);
    }

    fn destroy_subsurface(&mut self) {
        if let Some(mut state) = self.subsurface.take() {
            if let Some(bg) = state.bg_effect_surface.take() {
                bg.destroy();
            }
            state.subsurface.destroy();
            state.surface.destroy();
            self.layer_surface.wl_surface().commit();
            println!("Destroyed subsurface");
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

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let new_width = if configure.new_size.0 > 0 {
            configure.new_size.0
        } else {
            LAYER_W
        };
        let new_height = if configure.new_size.1 > 0 {
            configure.new_size.1
        } else {
            LAYER_H
        };

        let size_changed = new_width != self.width || new_height != self.height;
        self.width = new_width;
        self.height = new_height;

        if size_changed || self.first_configure {
            self.first_configure = false;
            self.buffer = None;
            self.update_blur_region(qh);
            self.draw_layer_surface();
        }
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {}

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        popup: &Popup,
        _config: PopupConfigure,
    ) {
        if self
            .parent_popup
            .as_ref()
            .is_some_and(|p| &p.popup == popup)
        {
            self.parent_popup.as_mut().unwrap().configured = true;
            self.parent_popup.as_mut().unwrap().buffer = None;
            self.draw_parent_popup();
            // Now open the child popup.
            self.open_child_popup(qh);
        } else if self.child_popup.as_ref().is_some_and(|p| &p.popup == popup) {
            self.child_popup.as_mut().unwrap().configured = true;
            self.child_popup.as_mut().unwrap().buffer = None;
            self.draw_child_popup();
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self.child_popup.as_ref().is_some_and(|p| &p.popup == popup) {
            if let Some(mut child) = self.child_popup.take() {
                if let Some(bg) = child.bg_effect_surface.take() {
                    bg.destroy();
                }
            }
        } else if self
            .parent_popup
            .as_ref()
            .is_some_and(|p| &p.popup == popup)
        {
            self.close_popups();
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
            // Popup controls (same as popup_background_effect).
            Keysym::w => {
                self.popup_surface_w = self.popup_surface_w.saturating_add(10);
                println!("popup surface width = {}", self.popup_surface_w);
                self.update_popups(qh);
            }
            Keysym::s => {
                self.popup_surface_w = self
                    .popup_surface_w
                    .saturating_sub(10)
                    .max(self.geometry_inset * 2 + 10);
                println!("popup surface width = {}", self.popup_surface_w);
                self.update_popups(qh);
            }
            Keysym::e => {
                self.popup_surface_h = self.popup_surface_h.saturating_add(10);
                println!("popup surface height = {}", self.popup_surface_h);
                self.update_popups(qh);
            }
            Keysym::d => {
                self.popup_surface_h = self
                    .popup_surface_h
                    .saturating_sub(10)
                    .max(self.geometry_inset * 2 + 10);
                println!("popup surface height = {}", self.popup_surface_h);
                self.update_popups(qh);
            }
            Keysym::r => {
                self.geometry_inset = self.geometry_inset.saturating_add(5);
                if self.geometry_inset * 2 + 10 > self.popup_surface_w {
                    self.popup_surface_w = self.geometry_inset * 2 + 10;
                }
                if self.geometry_inset * 2 + 10 > self.popup_surface_h {
                    self.popup_surface_h = self.geometry_inset * 2 + 10;
                }
                println!("geometry inset = {}", self.geometry_inset);
                self.update_popups(qh);
            }
            Keysym::f => {
                self.geometry_inset = self.geometry_inset.saturating_sub(5);
                println!("geometry inset = {}", self.geometry_inset);
                self.update_popups(qh);
            }
            Keysym::t => {
                self.popup_buffer_scale = (self.popup_buffer_scale + 1).min(4);
                println!("popup buffer scale = {}", self.popup_buffer_scale);
                self.update_popups(qh);
            }
            Keysym::g => {
                self.popup_buffer_scale = self.popup_buffer_scale.saturating_sub(1).max(1);
                println!("popup buffer scale = {}", self.popup_buffer_scale);
                self.update_popups(qh);
            }
            // Layer switching.
            Keysym::Page_Up => {
                if let Some(layer) = self.layer_up() {
                    self.set_layer(layer);
                }
            }
            Keysym::Page_Down => {
                if let Some(layer) = self.layer_down() {
                    self.set_layer(layer);
                }
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
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer_surface.wl_surface() {
                continue;
            }

            match event.kind {
                // Left click: toggle popups.
                PointerEventKind::Press { button: 0x110, .. } => {
                    if self.parent_popup.is_some() {
                        self.close_popups();
                    } else {
                        self.click_pos = (event.position.0 as i32, event.position.1 as i32);
                        self.open_popups(qh);
                    }
                }
                // Right click: toggle subsurface.
                PointerEventKind::Press { button: 0x111, .. } => {
                    let x = event.position.0 as i32;
                    let y = event.position.1 as i32;
                    if self.subsurface.is_some() {
                        self.destroy_subsurface();
                    } else {
                        self.create_subsurface(x, y, qh);
                    }
                }
                _ => {}
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
delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_xdg_popup!(App);
delegate_subcompositor!(App);
delegate_background_effect!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
