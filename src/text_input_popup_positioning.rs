use sctk::compositor::{CompositorHandler, CompositorState};
use sctk::output::{OutputHandler, OutputState};
use sctk::reexports::calloop::EventLoop;
use sctk::reexports::calloop_wayland_source::WaylandSource;
use sctk::reexports::client::globals::registry_queue_init;
use sctk::reexports::client::protocol::{
    wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_subsurface, wl_surface,
};
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use sctk::reexports::protocols::xdg::shell::client::xdg_positioner::{
    Anchor as PopupAnchor, ConstraintAdjustment, Gravity,
};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use sctk::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::shell::WaylandSurface;
use sctk::shell::wlr_layer::{
    Anchor as LayerAnchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler,
    LayerSurface, LayerSurfaceConfigure,
};
use sctk::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use sctk::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use sctk::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use sctk::shm::slot::{Buffer, SlotPool};
use sctk::shm::{Shm, ShmHandler};
use sctk::subcompositor::SubcompositorState;
use sctk::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor, delegate_xdg_popup,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use zwp_text_input_v3::ZwpTextInputV3;

// Toplevel: the surface is larger than the window geometry by WINDOW_INSET on every side.
const WINDOW_INSET: i32 = 24;
const WINDOW_W: i32 = 440;
const WINDOW_H: i32 = 240;

// Layer surface: no geometry, the field is flush with the surface edges.
const LAYER_W: i32 = 440;
const LAYER_H: i32 = 200;

const SUB_W: i32 = 260;
const SUB_H: i32 = 72;

const POPUP_INSET: i32 = 16;
const POPUP_W: i32 = 280;
const POPUP_H: i32 = 96;

const FIELD_H: i32 = 28;
const FIELD_PAD: i32 = 4;
const CELL_W: i32 = 10;
const CURSOR_W: i32 = 2;

const CELL_COLORS: [u32; 8] = [
    0xff_d0_40_40,
    0xff_d0_90_30,
    0xff_c0_c0_30,
    0xff_50_b0_50,
    0xff_30_a0_b0,
    0xff_40_60_d0,
    0xff_90_50_d0,
    0xff_d0_50_a0,
];

const SUB_POSITION_NAMES: [&str; 4] = [
    "inside",
    "flush with the bottom-right corner",
    "overhanging the bottom-right corner",
    "overhanging the top-left corner",
];

fn main() {
    let layer_mode = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("layer") => true,
        Some(_) => {
            eprintln!("usage: text_input_popup_positioning [layer]");
            std::process::exit(1);
        }
    };

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
    let text_input_manager = globals
        .bind::<ZwpTextInputManagerV3, _, _>(&qh, 1..=1, ())
        .map_err(|err| println!("WARNING: zwp_text_input_manager_v3 not available: {err}"))
        .ok();

    let surface = compositor.create_surface(&qh);
    let (root, inset, width, height) = if layer_mode {
        let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell not available");
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Top,
            Some("text_input_popup_positioning"),
            None,
        );
        layer.set_size(LAYER_W as u32, LAYER_H as u32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        (Root::Layer(layer), 0, LAYER_W, LAYER_H)
    } else {
        let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
        window.set_title("text_input_popup_positioning");
        window.set_min_size(Some((SUB_W as u32 + 40, SUB_H as u32 + 80)));
        (Root::Window(window), WINDOW_INSET, WINDOW_W, WINDOW_H)
    };

    let (subsurface, sub_surface) = subcompositor.create_subsurface(root.wl_surface().clone(), &qh);

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        xdg_shell,
        shm,
        text_input_manager,

        exit: false,
        pool,
        root,
        configured: false,
        inset,
        width,
        height,
        main_buffer: None,
        sub: Sub {
            subsurface,
            surface: sub_surface,
            buffer: None,
            position_index: 0,
            position: (0, 0),
        },
        popup: None,

        fields: [
            TextField::new(Rect::default()),
            TextField::new(Rect {
                x: 6,
                y: SUB_H - FIELD_H - 6,
                w: SUB_W - 12,
                h: FIELD_H,
            }),
            TextField::new(Rect {
                x: POPUP_INSET + 8,
                y: POPUP_INSET + POPUP_H - FIELD_H - 8,
                w: POPUP_W - 16,
                h: FIELD_H,
            }),
        ],
        active: FieldId::Main,
        last_root_active: FieldId::Main,

        seat: None,
        keyboard: None,
        pointer: None,
        modifiers: Modifiers::default(),
        pointer_pos: (0, 0),

        text_input: None,
        text_input_surface: None,
        text_input_enabled: false,
        text_input_sent: None,
        pending_ime: PendingIme::default(),
    };
    app.update_layout();
    app.root.commit();

    println!("=== text_input_popup_positioning test ===");
    if layer_mode {
        println!("Layer surface without geometry. Fields are flush with the surface edges.");
        println!("Ctrl+1..9: anchor the layer surface (numpad layout, 5 = centered).");
    } else {
        println!("Toplevel with {WINDOW_INSET} px of shadow around the window geometry.");
    }
    println!("A subsurface with its own field is placed at an offset from the main surface.");
    println!("Left click: focus a field and move its cursor.");
    println!(
        "Right click: open a grabbing popup (with {POPUP_INSET} px of shadow) at the pointer."
    );
    println!("Ctrl+P: open the popup at the last pointer position (keyboard serial grab).");
    println!("Tab: switch between the main and subsurface fields.");
    println!("Ctrl+S: cycle the subsurface position.");
    println!("Typing, Left/Right/Home/End, Backspace/Delete edit the field.");
    println!("Escape: close the popup. Ctrl+Q: quit.");

    while !app.exit {
        event_loop.dispatch(None, &mut app).unwrap();
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldId {
    Main = 0,
    Sub = 1,
    Popup = 2,
}

struct Preedit {
    text: String,
    // Byte offset of the cursor inside the preedit, None if hidden.
    cursor: Option<usize>,
}

struct TextField {
    rect: Rect,
    text: String,
    // Byte offset into text.
    cursor: usize,
    preedit: Option<Preedit>,
}

impl TextField {
    fn new(rect: Rect) -> Self {
        Self {
            rect,
            text: String::new(),
            cursor: 0,
            preedit: None,
        }
    }

    fn capacity(&self) -> usize {
        ((self.rect.w - FIELD_PAD * 2) / CELL_W).max(0) as usize
    }

    fn insert(&mut self, s: &str) {
        let room = self.capacity().saturating_sub(self.text.chars().count());
        let end = s.char_indices().nth(room).map_or(s.len(), |(i, _)| i);
        let s = &s[..end];
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .chars()
            .next()
            .map_or(pos, |c| pos + c.len_utf8())
    }

    fn delete_surrounding(&mut self, before: usize, after: usize) {
        let mut start = self.cursor.saturating_sub(before);
        while !self.text.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = (self.cursor + after).min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end += 1;
        }
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    fn set_cursor_from_x(&mut self, x: i32) {
        let cell = ((x - self.rect.x - FIELD_PAD + CELL_W / 2) / CELL_W).max(0) as usize;
        self.cursor = self
            .text
            .char_indices()
            .nth(cell)
            .map_or(self.text.len(), |(i, _)| i);
    }

    // Cursor rectangle in the field's surface-local coordinates.
    fn cursor_rect(&self) -> Rect {
        let mut cells = self.text[..self.cursor].chars().count();
        if let Some(preedit) = &self.preedit {
            cells += preedit.text[..preedit.cursor.unwrap_or(0)].chars().count();
        }
        let x = self.rect.x + FIELD_PAD + cells as i32 * CELL_W;
        Rect {
            x: x.min(self.rect.x + self.rect.w - FIELD_PAD),
            y: self.rect.y + FIELD_PAD,
            w: CURSOR_W,
            h: self.rect.h - FIELD_PAD * 2,
        }
    }

    fn draw(&self, canvas: &mut Canvas, active: bool, ime_enabled: bool) {
        let r = self.rect;
        let border = match (active, ime_enabled) {
            (true, true) => 0xff_30_a0_ff,
            (true, false) => 0xff_ff_a0_30,
            (false, _) => 0xff_60_60_60,
        };
        canvas.fill(r, border);
        let inner = Rect {
            x: r.x + 2,
            y: r.y + 2,
            w: r.w - 4,
            h: r.h - 4,
        };
        canvas.fill(inner, 0xff_f0_f0_f0);
        canvas.push_clip(inner);

        let cell_y = r.y + FIELD_PAD + 2;
        let cell_h = r.h - FIELD_PAD * 2 - 4;
        let draw_cell = |canvas: &mut Canvas, index: usize, c: char, preedit: bool| {
            let x = r.x + FIELD_PAD + index as i32 * CELL_W;
            if !c.is_whitespace() {
                let mut color = CELL_COLORS[c as usize % CELL_COLORS.len()];
                if preedit {
                    // Blend halfway towards white.
                    color = 0xff_00_00_00 | ((color & 0xfe_fe_fe) >> 1) + 0x80_80_80;
                }
                canvas.fill(
                    Rect {
                        x: x + 1,
                        y: cell_y,
                        w: CELL_W - 2,
                        h: cell_h,
                    },
                    color,
                );
            }
            if preedit {
                canvas.fill(
                    Rect {
                        x,
                        y: cell_y + cell_h + 1,
                        w: CELL_W,
                        h: 2,
                    },
                    0xff_20_20_20,
                );
            }
        };

        let (before, after) = self.text.split_at(self.cursor);
        let mut index = 0;
        for c in before.chars() {
            draw_cell(canvas, index, c, false);
            index += 1;
        }
        if let Some(preedit) = &self.preedit {
            for c in preedit.text.chars() {
                draw_cell(canvas, index, c, true);
                index += 1;
            }
        }
        for c in after.chars() {
            draw_cell(canvas, index, c, false);
            index += 1;
        }

        let hidden = self.preedit.as_ref().is_some_and(|p| p.cursor.is_none());
        if active && !hidden {
            canvas.fill(self.cursor_rect(), 0xff_00_00_00);
        }

        canvas.pop_clip();
    }
}

struct Canvas<'a> {
    data: &'a mut [u8],
    width: i32,
    height: i32,
    clip: Vec<Rect>,
}

impl Canvas<'_> {
    fn fill(&mut self, r: Rect, color: u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
        x0 = x0.max(0);
        y0 = y0.max(0);
        x1 = x1.min(self.width);
        y1 = y1.min(self.height);
        if let Some(clip) = self.clip.last() {
            x0 = x0.max(clip.x);
            y0 = y0.max(clip.y);
            x1 = x1.min(clip.x + clip.w);
            y1 = y1.min(clip.y + clip.h);
        }
        let bytes = color.to_le_bytes();
        for y in y0..y1 {
            for x in x0..x1 {
                let offset = ((y * self.width + x) * 4) as usize;
                self.data[offset..offset + 4].copy_from_slice(&bytes);
            }
        }
    }

    fn push_clip(&mut self, r: Rect) {
        self.clip.push(r);
    }

    fn pop_clip(&mut self) {
        self.clip.pop();
    }
}

fn draw_surface(
    pool: &mut SlotPool,
    buffer: &mut Option<Buffer>,
    surface: &wl_surface::WlSurface,
    width: i32,
    height: i32,
    paint: impl FnOnce(&mut Canvas),
) {
    let stride = width * 4;
    let reusable = buffer
        .as_ref()
        .is_some_and(|b| b.height() == height && b.stride() == stride);
    if !reusable {
        *buffer = None;
    }
    let buffer = buffer.get_or_insert_with(|| {
        pool.create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .expect("create buffer")
            .0
    });
    let data = match pool.canvas(buffer) {
        Some(data) => data,
        None => {
            let (new_buffer, data) = pool
                .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
                .expect("create buffer");
            *buffer = new_buffer;
            data
        }
    };

    paint(&mut Canvas {
        data,
        width,
        height,
        clip: Vec::new(),
    });

    surface.damage_buffer(0, 0, width, height);
    buffer.attach_to(surface).expect("attach buffer");
}

enum Root {
    Window(Window),
    Layer(LayerSurface),
}

impl Root {
    fn wl_surface(&self) -> &wl_surface::WlSurface {
        match self {
            Root::Window(window) => window.wl_surface(),
            Root::Layer(layer) => layer.wl_surface(),
        }
    }

    fn commit(&self) {
        self.wl_surface().commit();
    }
}

struct Sub {
    subsurface: wl_subsurface::WlSubsurface,
    surface: wl_surface::WlSurface,
    buffer: Option<Buffer>,
    position_index: usize,
    position: (i32, i32),
}

struct PopupState {
    popup: Popup,
    buffer: Option<Buffer>,
    // Popup geometry position relative to the parent geometry, from the last configure.
    position: (i32, i32),
    configured: bool,
}

#[derive(Default)]
struct PendingIme {
    preedit: Option<(String, i32)>,
    commit: Option<String>,
    delete: Option<(u32, u32)>,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    xdg_shell: XdgShell,
    shm: Shm,
    text_input_manager: Option<ZwpTextInputManagerV3>,

    exit: bool,
    pool: SlotPool,
    root: Root,
    configured: bool,
    // Offset of the window geometry inside the root surface.
    inset: i32,
    // Window geometry size (or the layer surface size).
    width: i32,
    height: i32,
    main_buffer: Option<Buffer>,
    sub: Sub,
    popup: Option<PopupState>,

    fields: [TextField; 3],
    active: FieldId,
    // Last active field on the root surface, restored when the popup closes.
    last_root_active: FieldId,

    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    modifiers: Modifiers,
    // Last pointer position over the root or the subsurface, in root surface coordinates.
    pointer_pos: (i32, i32),

    text_input: Option<ZwpTextInputV3>,
    text_input_surface: Option<wl_surface::WlSurface>,
    text_input_enabled: bool,
    // Last committed surrounding text, cursor and cursor rectangle, to avoid redundant commits.
    // The IME responds to every commit with done, so committing unconditionally loops.
    text_input_sent: Option<(String, usize, Rect)>,
    pending_ime: PendingIme,
}

impl App {
    fn field(&self) -> &TextField {
        &self.fields[self.active as usize]
    }

    fn field_mut(&mut self) -> &mut TextField {
        &mut self.fields[self.active as usize]
    }

    // The surface that receives keyboard and text input focus for this field.
    fn focus_surface(&self, id: FieldId) -> Option<&wl_surface::WlSurface> {
        match id {
            FieldId::Main | FieldId::Sub => Some(self.root.wl_surface()),
            FieldId::Popup => self.popup.as_ref().map(|p| p.popup.wl_surface()),
        }
    }

    // Offset of the field's surface relative to its focus surface.
    fn field_surface_offset(&self, id: FieldId) -> (i32, i32) {
        match id {
            FieldId::Sub => self.sub.position,
            FieldId::Main | FieldId::Popup => (0, 0),
        }
    }

    // Offset to add to the active field's cursor rectangle before sending it.
    fn cursor_rect_offset(&self) -> (i32, i32) {
        if self.active == FieldId::Popup
            && self.text_input_surface.as_ref() == Some(self.root.wl_surface())
            && let Some(popup) = &self.popup
        {
            // The text input focus stayed on the parent: translate into parent surface coordinates.
            return (
                self.inset + popup.position.0 - POPUP_INSET,
                self.inset + popup.position.1 - POPUP_INSET,
            );
        }
        self.field_surface_offset(self.active)
    }

    fn field_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<FieldId> {
        if surface == self.root.wl_surface() {
            Some(FieldId::Main)
        } else if surface == &self.sub.surface {
            Some(FieldId::Sub)
        } else if self
            .popup
            .as_ref()
            .is_some_and(|p| p.popup.wl_surface() == surface)
        {
            Some(FieldId::Popup)
        } else {
            None
        }
    }

    fn update_layout(&mut self) {
        let (inset, w, h) = (self.inset, self.width, self.height);
        self.fields[FieldId::Main as usize].rect = Rect {
            x: inset + 4,
            y: inset + 4,
            w: w - 8,
            h: FIELD_H,
        };

        let position = match self.sub.position_index {
            0 => (inset + 20, inset + 48),
            1 => (inset + w - SUB_W, inset + h - SUB_H),
            2 => (inset + w - SUB_W / 2, inset + h - SUB_H / 2),
            _ => (inset - SUB_W / 2, inset - SUB_H / 2),
        };
        self.sub.position = position;
        self.sub.subsurface.set_position(position.0, position.1);
    }

    fn draw_main(&mut self) {
        if !self.configured {
            return;
        }

        let (inset, w, h) = (self.inset, self.width, self.height);
        let surface_w = w + inset * 2;
        let surface_h = h + inset * 2;
        let active = self.active == FieldId::Main;
        let enabled = self.text_input_enabled;
        let field = &self.fields[FieldId::Main as usize];
        draw_surface(
            &mut self.pool,
            &mut self.main_buffer,
            self.root.wl_surface(),
            surface_w,
            surface_h,
            |canvas| {
                // Translucent shadow outside the geometry.
                canvas.fill(
                    Rect {
                        x: 0,
                        y: 0,
                        w: surface_w,
                        h: surface_h,
                    },
                    0x40_00_00_00,
                );
                let geometry = Rect {
                    x: inset,
                    y: inset,
                    w,
                    h,
                };
                canvas.fill(geometry, 0xff_30_30_38);
                field.draw(canvas, active, enabled);
            },
        );

        if let Root::Window(window) = &self.root {
            window.xdg_surface().set_window_geometry(inset, inset, w, h);
        }
        self.root.commit();
    }

    fn draw_sub(&mut self) {
        if !self.configured {
            return;
        }

        let active = self.active == FieldId::Sub;
        let enabled = self.text_input_enabled;
        let field = &self.fields[FieldId::Sub as usize];
        draw_surface(
            &mut self.pool,
            &mut self.sub.buffer,
            &self.sub.surface,
            SUB_W,
            SUB_H,
            |canvas| {
                canvas.fill(
                    Rect {
                        x: 0,
                        y: 0,
                        w: SUB_W,
                        h: SUB_H,
                    },
                    0xff_20_60_60,
                );
                field.draw(canvas, active, enabled);
            },
        );
        self.sub.surface.commit();
        // The subsurface is synchronized, apply its state.
        self.root.commit();
    }

    fn draw_popup(&mut self) {
        let Some(state) = &mut self.popup else {
            return;
        };
        if !state.configured {
            return;
        }

        let surface_w = POPUP_W + POPUP_INSET * 2;
        let surface_h = POPUP_H + POPUP_INSET * 2;
        let active = self.active == FieldId::Popup;
        let enabled = self.text_input_enabled;
        let field = &self.fields[FieldId::Popup as usize];
        draw_surface(
            &mut self.pool,
            &mut state.buffer,
            state.popup.wl_surface(),
            surface_w,
            surface_h,
            |canvas| {
                canvas.fill(
                    Rect {
                        x: 0,
                        y: 0,
                        w: surface_w,
                        h: surface_h,
                    },
                    0x40_00_00_00,
                );
                canvas.fill(
                    Rect {
                        x: POPUP_INSET,
                        y: POPUP_INSET,
                        w: POPUP_W,
                        h: POPUP_H,
                    },
                    0xff_50_30_60,
                );
                field.draw(canvas, active, enabled);
            },
        );
        state.popup.wl_surface().commit();
    }

    fn draw_field(&mut self, id: FieldId) {
        match id {
            FieldId::Main => self.draw_main(),
            FieldId::Sub => self.draw_sub(),
            FieldId::Popup => self.draw_popup(),
        }
    }

    fn draw_all(&mut self) {
        self.draw_sub();
        self.draw_main();
        self.draw_popup();
    }

    fn set_active(&mut self, id: FieldId) {
        if self.active == id {
            return;
        }

        let old = self.active;
        self.field_mut().preedit = None;
        self.active = id;
        if id != FieldId::Popup {
            self.last_root_active = id;
        }
        self.sync_text_input(true, zwp_text_input_v3::ChangeCause::Other);
        self.draw_field(old);
        self.draw_field(id);
    }

    fn sync_text_input(&mut self, reset: bool, cause: zwp_text_input_v3::ChangeCause) {
        let Some(text_input) = &self.text_input else {
            return;
        };

        // Some compositors (e.g. sway) keep the keyboard and text input focus on the parent
        // while a grabbing popup is open, so the popup field also works with focus on the root
        // (like GTK popovers).
        let want_enabled = self.text_input_surface.as_ref().is_some_and(|s| {
            Some(s) == self.focus_surface(self.active)
                || (self.active == FieldId::Popup && s == self.root.wl_surface())
        });

        if want_enabled {
            if !self.text_input_enabled || reset {
                text_input.enable();
                text_input.set_content_type(
                    zwp_text_input_v3::ContentHint::None,
                    zwp_text_input_v3::ContentPurpose::Normal,
                );
                self.text_input_sent = None;
            }
            self.text_input_enabled = true;

            let field = self.field();
            let (dx, dy) = self.cursor_rect_offset();
            let mut r = field.cursor_rect();
            r.x += dx;
            r.y += dy;
            let state = (field.text.clone(), field.cursor, r);
            if self.text_input_sent.as_ref() == Some(&state) {
                return;
            }

            let cursor = field.cursor as i32;
            text_input.set_surrounding_text(field.text.clone(), cursor, cursor);
            text_input.set_text_change_cause(cause);
            text_input.set_cursor_rectangle(r.x, r.y, r.w, r.h);
            text_input.commit();
            self.text_input_sent = Some(state);
            let Rect { x, y, .. } = r;
            println!(
                "text input: {:?} field, cursor rectangle {}x{} at ({x}, {y}) on {}",
                self.active,
                r.w,
                r.h,
                self.text_input_surface.as_ref().unwrap().id(),
            );
        } else if self.text_input_enabled {
            text_input.disable();
            text_input.commit();
            self.text_input_enabled = false;
            println!("text input: disabled");
        }
    }

    fn apply_ime(&mut self) {
        let pending = std::mem::take(&mut self.pending_ime);
        let field = self.field_mut();

        field.preedit = None;
        if let Some((before, after)) = pending.delete {
            field.delete_surrounding(before as usize, after as usize);
        }
        if let Some(text) = &pending.commit {
            field.insert(text);
        }
        if let Some((text, cursor)) = pending.preedit {
            let cursor = usize::try_from(cursor)
                .ok()
                .filter(|&c| text.is_char_boundary(c));
            field.preedit = Some(Preedit { text, cursor });
        }

        if self.text_input_enabled {
            self.sync_text_input(false, zwp_text_input_v3::ChangeCause::InputMethod);
        }
        self.draw_field(self.active);
    }

    fn edit(&mut self, f: impl FnOnce(&mut TextField)) {
        let field = self.field_mut();
        field.preedit = None;
        f(field);
        self.sync_text_input(false, zwp_text_input_v3::ChangeCause::Other);
        self.draw_field(self.active);
    }

    fn open_popup(&mut self, qh: &QueueHandle<Self>, serial: u32) {
        self.close_popup();
        let Some(seat) = &self.seat else {
            return;
        };

        // The anchor rectangle is relative to the parent window geometry, and must be inside.
        let x = (self.pointer_pos.0 - self.inset).clamp(0, self.width - 1);
        let y = (self.pointer_pos.1 - self.inset).clamp(0, self.height - 1);

        let positioner = XdgPositioner::new(&self.xdg_shell).expect("create positioner");
        positioner.set_size(POPUP_W, POPUP_H);
        positioner.set_anchor_rect(x, y, 1, 1);
        positioner.set_anchor(PopupAnchor::BottomRight);
        positioner.set_gravity(Gravity::BottomRight);
        positioner.set_constraint_adjustment(
            ConstraintAdjustment::SlideX
                | ConstraintAdjustment::SlideY
                | ConstraintAdjustment::FlipX
                | ConstraintAdjustment::FlipY,
        );

        let parent = match &self.root {
            Root::Window(window) => Some(window.xdg_surface()),
            Root::Layer(_) => None,
        };
        let popup = Popup::from_surface(
            parent,
            &positioner,
            qh,
            self.compositor.create_surface(qh),
            &self.xdg_shell,
        )
        .expect("create popup");
        if let Root::Layer(layer) = &self.root {
            layer.get_popup(popup.xdg_popup());
        }
        popup
            .xdg_surface()
            .set_window_geometry(POPUP_INSET, POPUP_INSET, POPUP_W, POPUP_H);
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().commit();

        self.popup = Some(PopupState {
            popup,
            buffer: None,
            position: (x, y),
            configured: false,
        });
        println!("popup: opened, anchored at ({x}, {y}) relative to parent geometry");
        self.set_active(FieldId::Popup);
    }

    fn close_popup(&mut self) {
        let Some(state) = self.popup.take() else {
            return;
        };
        println!("popup: closed");

        self.fields[FieldId::Popup as usize].preedit = None;
        if self.text_input_surface.as_ref() == Some(state.popup.wl_surface()) {
            // The text input focus surface is gone.
            self.text_input_surface = None;
            self.text_input_enabled = false;
        }
        self.set_active(self.last_root_active);
    }

    fn cycle_sub_position(&mut self) {
        self.sub.position_index = (self.sub.position_index + 1) % SUB_POSITION_NAMES.len();
        self.update_layout();
        println!(
            "subsurface: {} at {:?}",
            SUB_POSITION_NAMES[self.sub.position_index], self.sub.position
        );
        self.root.commit();
        if self.active == FieldId::Sub {
            self.sync_text_input(false, zwp_text_input_v3::ChangeCause::Other);
        }
    }

    fn set_layer_anchor(&mut self, digit: u32) {
        let Root::Layer(layer) = &self.root else {
            return;
        };

        let anchor = match digit {
            1 => LayerAnchor::BOTTOM | LayerAnchor::LEFT,
            2 => LayerAnchor::BOTTOM,
            3 => LayerAnchor::BOTTOM | LayerAnchor::RIGHT,
            4 => LayerAnchor::LEFT,
            6 => LayerAnchor::RIGHT,
            7 => LayerAnchor::TOP | LayerAnchor::LEFT,
            8 => LayerAnchor::TOP,
            9 => LayerAnchor::TOP | LayerAnchor::RIGHT,
            _ => LayerAnchor::empty(),
        };
        println!("layer: anchor {anchor:?}");
        layer.set_anchor(anchor);
        layer.commit();
    }

    fn handle_key(&mut self, qh: &QueueHandle<Self>, serial: u32, event: &KeyEvent, repeat: bool) {
        if self.modifiers.ctrl {
            if repeat {
                return;
            }
            match event.keysym {
                Keysym::q => self.exit = true,
                Keysym::s => self.cycle_sub_position(),
                Keysym::p if self.popup.is_none() => self.open_popup(qh, serial),
                keysym => {
                    if let Some(digit) = keysym.key_char().and_then(|c| c.to_digit(10)) {
                        self.set_layer_anchor(digit);
                    }
                }
            }
            return;
        }

        match event.keysym {
            Keysym::Escape => self.close_popup(),
            Keysym::Tab | Keysym::ISO_Left_Tab => match self.active {
                FieldId::Main => self.set_active(FieldId::Sub),
                FieldId::Sub => self.set_active(FieldId::Main),
                FieldId::Popup => {}
            },
            Keysym::Left => self.edit(|f| f.cursor = f.prev_boundary(f.cursor)),
            Keysym::Right => self.edit(|f| f.cursor = f.next_boundary(f.cursor)),
            Keysym::Home => self.edit(|f| f.cursor = 0),
            Keysym::End => self.edit(|f| f.cursor = f.text.len()),
            Keysym::BackSpace => self.edit(|f| {
                let start = f.prev_boundary(f.cursor);
                f.delete_surrounding(f.cursor - start, 0);
            }),
            Keysym::Delete => self.edit(|f| {
                let end = f.next_boundary(f.cursor);
                f.delete_surrounding(0, end - f.cursor);
            }),
            _ => {
                if let Some(text) = &event.utf8
                    && !text.chars().any(char::is_control)
                {
                    self.edit(|f| f.insert(text));
                }
            }
        }
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for App {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: zwp_text_input_manager_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_text_input_v3::Event;
        match event {
            Event::Enter { surface } => {
                println!("text input: enter {}", surface.id());
                state.text_input_surface = Some(surface);
                state.text_input_enabled = false;
                state.sync_text_input(true, zwp_text_input_v3::ChangeCause::Other);
                state.draw_field(state.active);
            }
            Event::Leave { surface } => {
                println!("text input: leave {}", surface.id());
                if state.text_input_surface.as_ref() == Some(&surface) {
                    state.text_input_surface = None;
                    state.sync_text_input(false, zwp_text_input_v3::ChangeCause::Other);
                    state.field_mut().preedit = None;
                    state.draw_field(state.active);
                }
            }
            Event::PreeditString {
                text, cursor_begin, ..
            } => {
                state.pending_ime.preedit = text.map(|text| (text, cursor_begin));
            }
            Event::CommitString { text } => {
                state.pending_ime.commit = text;
            }
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.pending_ime.delete = Some((before_length, after_length));
            }
            Event::Done { .. } => state.apply_ime(),
            _ => {}
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
        let width = configure.new_size.0.map_or(WINDOW_W, |v| v.get() as i32);
        let height = configure.new_size.1.map_or(WINDOW_H, |v| v.get() as i32);
        let changed = width != self.width || height != self.height;
        if self.configured && !changed {
            return;
        }

        self.configured = true;
        self.width = width;
        self.height = height;
        self.update_layout();
        self.draw_all();
        if self.active != FieldId::Popup {
            self.sync_text_input(false, zwp_text_input_v3::ChangeCause::Other);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let width = match configure.new_size.0 {
            0 => LAYER_W,
            w => w as i32,
        };
        let height = match configure.new_size.1 {
            0 => LAYER_H,
            h => h as i32,
        };
        let changed = width != self.width || height != self.height;
        if self.configured && !changed {
            // Still need to commit to ack the configure.
            self.root.commit();
            return;
        }

        self.configured = true;
        self.width = width;
        self.height = height;
        self.update_layout();
        self.draw_all();
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        if let Some(state) = &mut self.popup
            && &state.popup == popup
        {
            println!(
                "popup: configured at {:?} relative to parent geometry",
                configure.position
            );
            state.position = configure.position;
            state.configured = true;
            self.draw_popup();
            if self.active == FieldId::Popup {
                self.sync_text_input(false, zwp_text_input_v3::ChangeCause::Other);
            }
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if self.popup.as_ref().is_some_and(|p| &p.popup == popup) {
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
        // new_seat() isn't called for seats that exist at startup.
        if self.seat.is_none() {
            if let Some(manager) = &self.text_input_manager {
                self.text_input = Some(manager.get_text_input(&seat, qh, ()));
            }
            self.seat = Some(seat.clone());
        }
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
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        match self.field_for_surface(surface) {
            Some(FieldId::Popup) => self.set_active(FieldId::Popup),
            Some(_) if self.active == FieldId::Popup && self.popup.is_none() => {
                self.set_active(self.last_root_active)
            }
            _ => {}
        }
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
        serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(qh, serial, &event, false);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(qh, serial, &event, true);
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
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
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
            let Some(id) = self.field_for_surface(&event.surface) else {
                continue;
            };
            let (x, y) = (event.position.0 as i32, event.position.1 as i32);
            if id != FieldId::Popup {
                let (dx, dy) = self.field_surface_offset(id);
                self.pointer_pos = (x + dx, y + dy);
            }

            let PointerEventKind::Press { button, serial, .. } = event.kind else {
                continue;
            };
            match button {
                // BTN_LEFT
                0x110 => {
                    self.set_active(id);
                    let field = &self.fields[id as usize];
                    let r = field.rect;
                    if (r.y..r.y + r.h).contains(&y) {
                        self.edit(|f| f.set_cursor_from_x(x));
                    }
                }
                // BTN_RIGHT
                0x111 if id != FieldId::Popup => self.open_popup(qh, serial),
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
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState,];
}
