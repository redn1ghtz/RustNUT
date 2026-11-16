mod controls;
pub mod gauges;
mod scroll;
pub mod shapes;
use super::storage::wide;
use crate::nut::Power;
use std::{
    cell::{Cell, RefCell},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute},
        Gdi::*,
    },
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemServices::{SS_ENDELLIPSIS, SS_NOPREFIX, SS_NOTIFY, SS_OWNERDRAW},
    },
    UI::{
        Controls::*,
        Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled},
        WindowsAndMessaging::*,
    },
};
use windows_sys::core::w;

pub const SETTINGS: u16 = 101;
pub const BACK: u16 = 102;
pub const SAVE: u16 = 103;
pub const CANCEL: u16 = 104;
pub const STATUS: u16 = 110;
pub const DETAIL: u16 = 111;
pub const VALUES: u16 = 112;
pub const HOST: u16 = 201;
pub const PORT: u16 = 202;
pub const UPS: u16 = 203;
pub const TLS: u16 = 204;
pub const USER: u16 = 205;
pub const SECRET: u16 = 206;
pub const POLL: u16 = 207;
pub const CHARGE: u16 = 208;
pub const SECONDS: u16 = 209;
pub const RU: u16 = 210;
pub const DARK: u16 = 211;
pub const NOTIFY: u16 = 212;
pub const START: u16 = 213;
pub const SHUTDOWN: u16 = 214;
pub const FORGET: u16 = 215;
pub const ERROR: u16 = 216;
pub const RUNTIME: u16 = 219;
pub const ACTION: u16 = 220;
pub const DELAY: u16 = 221;
pub const EXTRA: u16 = 222;
pub const EXTRA_DELAY: u16 = 223;
pub const IMMEDIATE: u16 = 224;
pub const FSD: u16 = 225;
pub const FREQUENCY: u16 = 226;
pub const RESET: u16 = 227;
pub const RANGE: u16 = 240;
pub const CONNECTION_TAB: u16 = 260;
pub const POWER_TAB: u16 = 261;
pub const LIMITS_TAB: u16 = 262;
pub const SUMMARY: u16 = 263;
pub const LIMITS_ENABLED: u16 = 265;
pub const GITHUB: u16 = 266;
pub const POWER_HELP: u16 = 267;
pub const ON_BATTERY: u16 = 268;
pub const FORCE_CLOSE: u16 = 269;
const TITLE: u16 = 900;
const SECTION: u16 = 901;
const SECONDARY: u16 = 902;
pub const FIELD_LABEL: u16 = 906;
pub const GROUP_TITLE: u16 = 907;
pub const GROUP_NOTE: u16 = 908;
pub const BATTERY: u16 = 909;

const LEFT_COLUMN: RECT = rect(28, 143, 380, 549);
const RIGHT_COLUMN: RECT = rect(396, 143, 748, 549);
const LIMITS_PANEL: RECT = rect(28, 143, 748, 549);

#[derive(Clone, Copy, PartialEq)]
pub enum Section {
    Always,
    Connection,
    Power,
    Limits,
}

pub fn tab_section(id: u16) -> Option<Section> {
    match id {
        CONNECTION_TAB => Some(Section::Connection),
        POWER_TAB => Some(Section::Power),
        LIMITS_TAB => Some(Section::Limits),
        _ => None,
    }
}

pub fn show_section(window: HWND, section: Section) {
    SECTION_SHOWN.set(section);
    without_redraw(window, || {
        // SAFETY: all grouped controls are owned by this thread.
        unsafe {
            CONTROLS.with_borrow(|controls| {
                for &(handle, _, _, group) in controls {
                    if group != Section::Always {
                        ShowWindow(handle, if group == section { SW_SHOWNA } else { SW_HIDE });
                    }
                }
            });
        }
    });
}

thread_local! {
    static REDRAW_PAUSED: Cell<bool> = const { Cell::new(false) };
    static TABLE_ROWS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub fn visible_for_update(window: HWND) -> bool {
    // SAFETY: synchronous read of the window style.
    REDRAW_PAUSED.get() || unsafe { IsWindowVisible(window) != 0 }
}

pub fn without_redraw<R>(window: HWND, update: impl FnOnce() -> R) -> R {
    // SAFETY: the temporary WM_SETREDRAW style change is paired on the same UI thread.
    unsafe {
        if REDRAW_PAUSED.get() || IsWindowVisible(window) == 0 {
            return update();
        }
        REDRAW_PAUSED.set(true);
        SendMessageW(window, WM_SETREDRAW, 0, 0);
        let result = update();
        SendMessageW(window, WM_SETREDRAW, 1, 0);
        REDRAW_PAUSED.set(false);
        RedrawWindow(
            window,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_NOERASE,
        );
        result
    }
}

fn is_edit(id: u16) -> bool {
    matches!(
        id,
        HOST | PORT | UPS | USER | SECRET | POLL | CHARGE | SECONDS | RUNTIME | DELAY | EXTRA_DELAY
    ) || (RANGE..RANGE + 2 * crate::config::RANGE_KEYS.len() as u16).contains(&id)
}

fn is_combo(id: u16) -> bool {
    matches!(id, TLS | RU | DARK | ACTION | FREQUENCY)
}

fn theme_control(control: HWND, id: u16, dark: bool) {
    // SAFETY: the control belongs to the UI thread and the theme strings are static.
    unsafe {
        if is_combo(id) {
            SetWindowTheme(
                control,
                if dark {
                    w!("DarkMode_Explorer")
                } else {
                    null()
                },
                null(),
            );
        } else if matches!(
            id,
            NOTIFY
                | START
                | SHUTDOWN
                | ON_BATTERY
                | FSD
                | IMMEDIATE
                | FORCE_CLOSE
                | EXTRA
                | LIMITS_ENABLED
        ) {
            let theme = if dark { w!("") } else { null() };
            SetWindowTheme(control, theme, theme);
        }
    }
}

pub const MIN_WIDTH: i32 = 710;
pub const MIN_HEIGHT: i32 = 690;

#[derive(Clone, Copy)]
pub struct Viewport {
    pub width: i32,
    pub height: i32,
    pub dpi: i32,
}

thread_local! {
    static VIEW: Cell<Viewport> = const { Cell::new(Viewport { width: 776, height: 710, dpi: 96 }) };
    static DASHBOARD: Cell<bool> = const { Cell::new(true) };
}

fn settings_rect(v: Viewport, r: RECT) -> RECT {
    let width = v.width.min(1120);
    let offset = (v.width - width) / 2;
    let x = |x| offset + x * width / 776;
    let y = |y| if y >= 549 { y + v.height - 631 } else { y };
    let left = x(r.left);
    let span = (r.right - r.left) * width / 776;
    rect(left, y(r.top), left + span, y(r.bottom))
}

fn layout_bounds(v: Viewport, id: u16, bounds: Bounds) -> Bounds {
    let (x, y, w, h) = bounds;
    if matches!(id, TITLE | SECONDARY) {
        return (x, y, w.min(v.width - 185 - x), h);
    }
    if DASHBOARD.get() {
        return gauges::position(v, id, bounds);
    }
    if id == BACK {
        return (v.width - 174, 26, 146, 39);
    }
    let r = settings_rect(v, rect(x, y, x + w, y + h));
    (r.left, r.top, r.right - r.left, r.bottom - r.top)
}

pub fn set_page(window: HWND, dashboard: bool) {
    DASHBOARD.set(dashboard);
    resize(window);
}

pub fn resize(window: HWND) {
    let mut r = RECT::default();
    // SAFETY: r is writable and each handle in CONTROLS belongs to this window.
    unsafe { GetClientRect(window, &mut r) };
    let dpi = VIEW.get().dpi;
    let v = Viewport {
        width: (r.right * 96 / dpi).max(MIN_WIDTH),
        height: (r.bottom * 96 / dpi).max(MIN_HEIGHT),
        dpi,
    };
    VIEW.set(v);
    CONTROLS.with_borrow(|controls| {
        if controls.is_empty() {
            return;
        }
        // SAFETY: the UI owns all child handles. DeferWindowPos batches their movement.
        unsafe {
            let mut moves = BeginDeferWindowPos(controls.len() as i32);
            for &(handle, id, bounds, _) in controls {
                let (x, y, w, h) = layout_bounds(v, id, bounds);
                let px = |n| scaled(n, dpi);
                if !moves.is_null() {
                    moves = DeferWindowPos(
                        moves,
                        handle,
                        null_mut(),
                        px(x),
                        px(y),
                        px(w),
                        px(h),
                        SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOREDRAW,
                    );
                }
            }
            if moves.is_null() {
                for &(handle, id, bounds, _) in controls {
                    position(handle, layout_bounds(v, id, bounds), dpi);
                }
            } else {
                EndDeferWindowPos(moves);
            }
        }
    });
    // SAFETY: redraw is queued for the live parent and its children.
    unsafe { RedrawWindow(window, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN) };
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF::from(r) | (COLORREF::from(g) << 8) | (COLORREF::from(b) << 16)
}

struct Theme {
    logo: HICON,
    background: HBRUSH,
    panel: HBRUSH,
    field: HBRUSH,
    regular: HFONT,
    heading: HFONT,
    large: HFONT,
    text: COLORREF,
    muted: COLORREF,
    accent: COLORREF,
    border: COLORREF,
    warning: COLORREF,
    field_color: COLORREF,
    panel_color: COLORREF,
    alternating: COLORREF,
    selected: COLORREF,
    on_accent: COLORREF,
    dark: bool,
    dpi: i32,
}

thread_local! { static THEME: RefCell<Option<Theme>> = const { RefCell::new(None) }; }
thread_local! { static CONTROLS: RefCell<Vec<(HWND, u16, Bounds, Section)>> = const { RefCell::new(Vec::new()) }; }
thread_local! { static SECTION_SHOWN: Cell<Section> = const { Cell::new(Section::Connection) }; }
thread_local! { static POWER: Cell<Power> = const { Cell::new(Power::Unknown) }; }
thread_local! { static METERS: Cell<[gauges::Meter; gauges::COUNT]> = Cell::new([gauges::Meter::default(); gauges::COUNT]); }

pub fn set_meters(window: HWND, state: Power, meters: [gauges::Meter; gauges::COUNT]) {
    let old = METERS.replace(meters);
    // SAFETY: the invalidated gauge controls belong to this window.
    unsafe {
        for (index, id) in gauges::VALUE_IDS.into_iter().enumerate() {
            if old[index] != meters[index] {
                let gauge = child(window, id);
                if !gauge.is_null() {
                    InvalidateRect(gauge, null(), 0);
                }
            }
        }
        if POWER.replace(state) != state {
            let v = VIEW.get();
            InvalidateRect(window, &at_dpi(status_dot(), v.dpi), 0);
        }
    }
}

impl Theme {
    fn new(dark: bool, dpi: i32) -> Self {
        let [
            background,
            text,
            accent,
            panel,
            field,
            muted,
            border,
            warning,
            alternating,
            selected,
        ] = if dark {
            [
                0x1b191a, 0xe9e4e0, 0x943a1f, 0x262223, 0x322a29, 0xb9aba3, 0x584742, 0xff9a7f,
                0x2c2525, 0x523126,
            ]
        } else {
            [
                0xf1f3f4, 0x000000, 0x000000, 0xe0e0e3, 0xf6f6f6, 0x45454a, 0xa2a3a8, 0x303034,
                0xe9e9ec, 0xcacbd0,
            ]
        }
        .map(hex);
        // SAFETY: GDI creates owned brushes/fonts from numeric parameters and a static font name.
        unsafe {
            let font = |height, weight| {
                CreateFontW(
                    -scaled(height, dpi),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    w!("Segoe UI"),
                )
            };
            Self {
                logo: LoadImageW(
                    GetModuleHandleW(null()),
                    super::ICON_RESOURCE,
                    IMAGE_ICON,
                    scaled(44, dpi),
                    scaled(44, dpi),
                    LR_DEFAULTCOLOR,
                ),
                background: CreateSolidBrush(background),
                panel: CreateSolidBrush(panel),
                field: CreateSolidBrush(field),
                regular: font(15, 400),
                heading: font(20, 650),
                large: font(30, 700),
                text,
                muted,
                accent,
                border,
                warning,
                alternating,
                selected,
                field_color: field,
                panel_color: panel,
                on_accent: if dark { text } else { hex(0xf6f6f6) },
                dark,
                dpi,
            }
        }
    }
}

impl Drop for Theme {
    fn drop(&mut self) {
        // SAFETY: owned GDI objects are dropped after their controls have been destroyed.
        unsafe {
            if !self.logo.is_null() {
                DestroyIcon(self.logo);
            }
            for object in [
                self.background,
                self.panel,
                self.field,
                self.regular,
                self.heading,
                self.large,
            ] {
                DeleteObject(object);
            }
        }
    }
}

pub fn scaled(n: i32, dpi: i32) -> i32 {
    (n * dpi + 48) / 96
}

fn hex(code: u32) -> COLORREF {
    rgb((code >> 16) as u8, (code >> 8) as u8, code as u8)
}

fn blend(color: COLORREF, target: COLORREF, percent: u32) -> COLORREF {
    let part = |shift| {
        let start = (color >> shift) & 255u32;
        let end = (target >> shift) & 255u32;
        ((start * (100 - percent) + end * percent) / 100u32) as u8
    };
    rgb(part(0), part(8), part(16))
}

fn field_label_id(id: u16) -> u16 {
    2000 + id
}

pub fn enable(window: HWND, id: u16, enabled: bool) {
    // SAFETY: both optional handles are children owned by this thread.
    unsafe {
        let control = child(window, id);
        if !control.is_null() && (IsWindowEnabled(control) != 0) != enabled {
            EnableWindow(control, i32::from(enabled));
            InvalidateRect(control, null(), 1);
            let label = child(window, field_label_id(id));
            if !label.is_null() {
                InvalidateRect(label, null(), 1);
            }
        }
    }
}

fn at_dpi(rect: RECT, dpi: i32) -> RECT {
    let s = |n| scaled(n, dpi);
    self::rect(s(rect.left), s(rect.top), s(rect.right), s(rect.bottom))
}

pub fn install(window: HWND, dark: bool, dpi: i32) {
    if THEME.with_borrow(|theme| {
        theme
            .as_ref()
            .is_some_and(|t| t.dark == dark && t.dpi == dpi)
    }) {
        return;
    }
    let previous = THEME.with_borrow_mut(|theme| theme.replace(Theme::new(dark, dpi)));
    let mode = i32::from(dark);
    VIEW.set(Viewport { dpi, ..VIEW.get() });
    resize(window);
    // SAFETY: DWM consumes an initialized BOOL during this call.
    unsafe {
        if DwmSetWindowAttribute(
            window,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&mode as *const i32).cast(),
            4,
        ) < 0
        {
            DwmSetWindowAttribute(window, 19, (&mode as *const i32).cast(), 4);
        }
        CONTROLS.with_borrow(|controls| {
            for &(child, id, _, _) in controls {
                apply_font(child, id);
                adjust_control(child, id, dpi);
                if previous.as_ref().is_some_and(|theme| theme.dark != dark) {
                    theme_control(child, id, dark);
                }
            }
        });
        RedrawWindow(
            window,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_FRAME,
        );
    }
    drop(previous);
}

pub fn cleanup() {
    THEME.with_borrow_mut(|theme| *theme = None);
}

pub fn handle(message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    // SAFETY: Windows owns message parameters for the specific synchronous messages below.
    unsafe {
        THEME.with_borrow(|theme| {
            let theme = theme.as_ref()?;
            match message {
                WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
                    let dc = wparam as HDC;
                    let id = GetDlgCtrlID(lparam as HWND) as u16;
                    let disabled = if id >= 2000 {
                        let field = child(GetParent(lparam as HWND), id - 2000);
                        !field.is_null() && IsWindowEnabled(field) == 0
                    } else {
                        IsWindowEnabled(lparam as HWND) == 0
                    };
                    let text = if disabled {
                        blend(theme.muted, theme.panel_color, 40)
                    } else {
                        match id {
                            ERROR => theme.warning,
                            DETAIL
                            | SECONDARY
                            | GROUP_NOTE
                            | POWER_HELP
                            | SUMMARY
                            | gauges::TABLE_KEY
                            | gauges::TABLE_VALUE
                            | gauges::IDENTITY => theme.muted,
                            _ => theme.text,
                        }
                    };
                    SetTextColor(dc, text);
                    let brush = if is_edit(id) || message == WM_CTLCOLORLISTBOX {
                        SetBkMode(dc, OPAQUE as i32);
                        SetBkColor(
                            dc,
                            if disabled {
                                theme.panel_color
                            } else {
                                theme.field_color
                            },
                        );
                        if disabled { theme.panel } else { theme.field }
                    } else {
                        SetBkMode(dc, TRANSPARENT as i32);
                        if in_panel(id) {
                            theme.panel
                        } else {
                            theme.background
                        }
                    };
                    Some(brush as LRESULT)
                }
                WM_DRAWITEM if lparam != 0 => {
                    let draw = &*(lparam as *const DRAWITEMSTRUCT);
                    match draw.CtlType {
                        ODT_BUTTON => draw_button(draw, theme),
                        ODT_LISTBOX if draw.CtlID as u16 == VALUES => draw_row(draw, theme),
                        ODT_COMBOBOX => draw_option(draw, theme),
                        ODT_STATIC => {
                            let index = gauges::VALUE_IDS
                                .iter()
                                .position(|&id| u32::from(id) == draw.CtlID)?;
                            gauges::paint(draw, theme, METERS.get()[index]);
                        }
                        _ => return None,
                    }
                    Some(1)
                }
                _ => None,
            }
        })
    }
}

pub fn print_background(window: HWND, dc: HDC, dashboard: bool) {
    let mut client = RECT::default();
    // SAFETY: client is writable and Windows supplies the DC for the paint message.
    unsafe { GetClientRect(window, &mut client) };
    paint_background(dc, &client, (0, 0), dashboard);
}

fn in_panel(id: u16) -> bool {
    matches!(
        id,
        STATUS
            | DETAIL
            | gauges::TABLE_KEY
            | gauges::TABLE_VALUE
            | gauges::RUNTIME_TEXT
            | gauges::IDENTITY
            | BATTERY
            | FIELD_LABEL
            | GROUP_TITLE
            | GROUP_NOTE
            | POWER_HELP
            | NOTIFY
            | START
            | SHUTDOWN
            | ON_BATTERY
            | FORCE_CLOSE
            | IMMEDIATE
            | EXTRA
            | FSD
            | LIMITS_ENABLED
    ) || (2000..3000).contains(&id)
}

fn rounded(dc: HDC, bounds: RECT, radius: i32, fill: COLORREF, stroke: COLORREF) {
    if shapes::rounded(dc, bounds, radius, fill, stroke) {
        return;
    }
    // SAFETY: these stock objects are borrowed only until the original objects are restored.
    unsafe {
        let brush = SelectObject(dc, GetStockObject(DC_BRUSH));
        let pen = SelectObject(dc, GetStockObject(DC_PEN));
        SetDCBrushColor(dc, fill);
        SetDCPenColor(dc, stroke);
        RoundRect(
            dc,
            bounds.left,
            bounds.top,
            bounds.right,
            bounds.bottom,
            radius,
            radius,
        );
        SelectObject(dc, pen);
        SelectObject(dc, brush);
    }
}

fn draw_text(dc: HDC, rect: &mut RECT, text: &[u16], font: HFONT, color: COLORREF, flags: u32) {
    // SAFETY: GDI reads only the supplied UTF-16 text, and the original font is restored.
    unsafe {
        let previous = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            text.as_ptr(),
            text.len() as i32,
            rect,
            DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS | flags,
        );
        SelectObject(dc, previous);
    }
}

fn draw_button(draw: &DRAWITEMSTRUCT, theme: &Theme) {
    let id = draw.CtlID as u16;
    let active = tab_section(id).is_some_and(|section| section == SECTION_SHOWN.get());
    let primary = matches!(id, SAVE | SETTINGS | CANCEL);
    let pressed = draw.itemState & ODS_SELECTED != 0;
    let fill = match (primary, active, pressed) {
        (true, _, true) => {
            if theme.dark {
                hex(0x78331f)
            } else {
                hex(0x303034)
            }
        }
        (true, _, false) => theme.accent,
        (_, true, _) | (_, _, true) => theme.selected,
        _ => theme.panel_color,
    };
    let stroke = if primary {
        fill
    } else if active {
        theme.warning
    } else {
        theme.border
    };
    // SAFETY: the button handle and paint DC belong to the live owner-draw message.
    unsafe {
        FillRect(
            draw.hDC,
            &draw.rcItem,
            if matches!(id, 217 | FORGET | GITHUB) {
                theme.panel
            } else {
                theme.background
            },
        );
        let disabled = draw.itemState & ODS_DISABLED != 0;
        let fill = if disabled { theme.panel_color } else { fill };
        rounded(
            draw.hDC,
            draw.rcItem,
            scaled(16, theme.dpi),
            fill,
            if disabled {
                blend(stroke, theme.panel_color, 55)
            } else {
                stroke
            },
        );
        let mut text = [0u16; 150];
        let len = GetWindowTextW(draw.hwndItem, text.as_mut_ptr(), text.len() as i32);
        let mut rect = draw.rcItem;
        draw_text(
            draw.hDC,
            &mut rect,
            &text[..len.max(0) as usize],
            theme.regular,
            if disabled {
                blend(theme.muted, theme.panel_color, 40)
            } else if primary {
                theme.on_accent
            } else {
                theme.text
            },
            DT_CENTER,
        );
        if draw.itemState & ODS_FOCUS != 0 {
            InflateRect(&mut rect, -4, -4);
            DrawFocusRect(draw.hDC, &rect);
        }
    }
}

fn draw_row(draw: &DRAWITEMSTRUCT, theme: &Theme) {
    // SAFETY: the list box with LBS_HASSTRINGS returns bounded UTF-16 items synchronously.
    unsafe {
        let selected = draw.itemState & ODS_SELECTED != 0;
        let fill = if selected {
            theme.selected
        } else if draw.itemID % 2 == 0 {
            theme.panel_color
        } else {
            theme.alternating
        };
        SetBkColor(draw.hDC, fill);
        ExtTextOutW(draw.hDC, 0, 0, ETO_OPAQUE, &draw.rcItem, null(), 0, null());
        if draw.itemID == u32::MAX {
            return;
        }
        let len = SendMessageW(draw.hwndItem, LB_GETTEXTLEN, draw.itemID as usize, 0);
        if !(0..=8192).contains(&len) {
            return;
        }
        let mut data = vec![0u16; len as usize + 1];
        SendMessageW(
            draw.hwndItem,
            LB_GETTEXT,
            draw.itemID as usize,
            data.as_mut_ptr() as LPARAM,
        );
        let text = &data[..len as usize];
        let Some(separator) = text.iter().position(|&c| c == 9) else {
            return;
        };
        let dc = draw.hDC;
        let r = draw.rcItem;
        let split = r.left + gauges::split(r.right - r.left);
        let color = if text.first() == Some(&u16::from(b'!')) {
            theme.warning
        } else {
            theme.text
        };
        draw_text(
            dc,
            &mut rect(r.left + scaled(12, theme.dpi), r.top, split, r.bottom),
            &text[..separator],
            theme.regular,
            color,
            DT_LEFT,
        );
        draw_text(
            dc,
            &mut rect(
                split + scaled(12, theme.dpi),
                r.top,
                r.right - scaled(6, theme.dpi),
                r.bottom,
            ),
            &text[separator + 1..],
            theme.regular,
            color,
            DT_LEFT,
        );
        if draw.itemState & ODS_FOCUS != 0 {
            DrawFocusRect(dc, &draw.rcItem);
        }
    }
}

fn draw_option(draw: &DRAWITEMSTRUCT, theme: &Theme) {
    // SAFETY: the string is copied from the non-editable combo into its exact capacity.
    unsafe {
        let selected = draw.itemState & ODS_SELECTED != 0;
        SetBkColor(
            draw.hDC,
            if selected {
                theme.selected
            } else {
                theme.field_color
            },
        );
        ExtTextOutW(draw.hDC, 0, 0, ETO_OPAQUE, &draw.rcItem, null(), 0, null());
        if draw.itemID == u32::MAX {
            return;
        }
        let len = SendMessageW(draw.hwndItem, CB_GETLBTEXTLEN, draw.itemID as usize, 0);
        if !(0..=128).contains(&len) {
            return;
        }
        let mut data = [0u16; 129];
        SendMessageW(
            draw.hwndItem,
            CB_GETLBTEXT,
            draw.itemID as usize,
            data.as_mut_ptr() as LPARAM,
        );
        let mut rect = draw.rcItem;
        rect.left += scaled(8, theme.dpi);
        draw_text(
            draw.hDC,
            &mut rect,
            &data[..len as usize],
            theme.regular,
            theme.text,
            DT_LEFT,
        );
        if draw.itemState & ODS_FOCUS != 0 {
            DrawFocusRect(draw.hDC, &draw.rcItem);
        }
    }
}

fn status_dot() -> RECT {
    rect(36, 109, 44, 117)
}

fn edit_frame(viewport: Viewport, id: u16, bounds: Bounds) -> RECT {
    let (x, y, width, height) = layout_bounds(viewport, id, bounds);
    rect(x - 8, y - 5, x + width + 8, y + height + 5)
}

pub fn invalidate_edit_frame(window: HWND, id: u16) {
    let viewport = VIEW.get();
    CONTROLS.with_borrow(|controls| {
        let section = SECTION_SHOWN.get();
        if let Some(&(_, _, bounds, _)) = controls
            .iter()
            .find(|&&(_, control_id, _, group)| control_id == id && group == section && is_edit(id))
        {
            let frame = at_dpi(edit_frame(viewport, id, bounds), viewport.dpi);
            // SAFETY: the dirty rectangle is in the live parent window's client coordinates.
            unsafe { InvalidateRect(window, &frame, 0) };
        }
    });
}

fn paint_background(dc: HDC, dirty: &RECT, origin: (i32, i32), dashboard: bool) {
    THEME.with_borrow(|theme| {
        let Some(t) = theme else { return };
        let s = |n| scaled(n, t.dpi);
        let scale = |r| {
            let r = at_dpi(r, t.dpi);
            rect(
                r.left - origin.0,
                r.top - origin.1,
                r.right - origin.0,
                r.bottom - origin.1,
            )
        };
        let visible = |r: RECT| {
            r.left < dirty.right
                && r.right > dirty.left
                && r.top < dirty.bottom
                && r.bottom > dirty.top
        };
        let frame = |bounds, radius, fill, stroke| {
            let bounds = scale(bounds);
            if visible(bounds) {
                rounded(dc, bounds, s(radius), fill, stroke);
            }
        };
        let panel = |bounds| frame(bounds, 24, t.panel_color, t.border);
        // SAFETY: paint shapes and stock GDI objects are scoped to this paint DC.
        unsafe {
            FillRect(dc, dirty, t.background);
            let logo = scale(rect(28, 23, 72, 67));
            if visible(logo) && !t.logo.is_null() {
                DrawIconEx(
                    dc,
                    logo.left,
                    logo.top,
                    t.logo,
                    logo.right - logo.left,
                    logo.bottom - logo.top,
                    0,
                    null_mut(),
                    DI_NORMAL,
                );
            }
            let v = VIEW.get();
            if dashboard {
                panel(gauges::hero(v));
                panel(gauges::table(v));
                let dot = match POWER.get() {
                    Power::Line => t.text,
                    Power::Battery => t.warning,
                    Power::Critical => t.accent,
                    Power::Unknown => t.muted,
                };
                frame(status_dot(), 8, dot, dot);
            } else {
                let panel = |bounds| panel(settings_rect(v, bounds));
                match SECTION_SHOWN.get() {
                    Section::Connection | Section::Power => {
                        panel(LEFT_COLUMN);
                        panel(RIGHT_COLUMN);
                    }
                    Section::Limits => panel(LIMITS_PANEL),
                    Section::Always => {}
                }
                CONTROLS.with_borrow(|controls| {
                    for &(control, id, bounds, section) in controls {
                        if section == SECTION_SHOWN.get() && is_edit(id) {
                            let focus = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus()
                                == control;
                            let enabled = IsWindowEnabled(control) != 0;
                            frame(
                                edit_frame(v, id, bounds),
                                10,
                                if enabled {
                                    t.field_color
                                } else {
                                    t.panel_color
                                },
                                if !enabled {
                                    blend(t.border, t.panel_color, 54)
                                } else if focus {
                                    t.warning
                                } else {
                                    t.border
                                },
                            );
                        }
                    }
                });
            }
        }
    });
}

pub fn paint(window: HWND, dashboard: bool) {
    // SAFETY: BeginPaint and EndPaint are paired during WM_PAINT.
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let dc = BeginPaint(window, &mut paint);
        let r = paint.rcPaint;
        let width = r.right - r.left;
        let height = r.bottom - r.top;
        if width <= 0 || height <= 0 {
            EndPaint(window, &paint);
            return;
        }
        let copy = CreateCompatibleDC(dc);
        let bitmap = if copy.is_null() {
            null_mut()
        } else {
            CreateCompatibleBitmap(dc, width, height)
        };
        if bitmap.is_null() {
            paint_background(dc, &r, (0, 0), dashboard);
        } else {
            let old = SelectObject(copy, bitmap);
            paint_background(copy, &rect(0, 0, width, height), (r.left, r.top), dashboard);
            BitBlt(dc, r.left, r.top, width, height, copy, 0, 0, SRCCOPY);
            SelectObject(copy, old);
            DeleteObject(bitmap);
        }
        if !copy.is_null() {
            DeleteDC(copy);
        }
        EndPaint(window, &paint);
    }
}

type Bounds = (i32, i32, i32, i32);

fn position(control: HWND, bounds: Bounds, dpi: i32) {
    let (x, y, width, height) = bounds;
    let s = |n| scaled(n, dpi);
    // SAFETY: control is a live direct child owned by this thread.
    unsafe { MoveWindow(control, s(x), s(y), s(width), s(height), 0) };
}

fn apply_font(control: HWND, id: u16) {
    let font = THEME.with_borrow(|theme| {
        let theme = theme.as_ref().unwrap();
        match id {
            TITLE => theme.large,
            STATUS | SECTION | GROUP_TITLE => theme.heading,
            _ if gauges::VALUE_IDS.contains(&id) => theme.heading,
            _ => theme.regular,
        }
    });
    // SAFETY: the font is retained in THEME until controls switch to its replacement.
    unsafe { SendMessageW(control, WM_SETFONT, font as WPARAM, 0) };
}

fn adjust_control(control: HWND, id: u16, dpi: i32) {
    // SAFETY: edit margins and item heights are passed as integers.
    unsafe {
        if is_edit(id) {
            SendMessageW(
                control,
                EM_SETMARGINS,
                (EC_LEFTMARGIN | EC_RIGHTMARGIN) as WPARAM,
                0,
            );
        } else if is_combo(id) {
            SendMessageW(control, CB_SETITEMHEIGHT, 0, scaled(29, dpi) as LPARAM);
            SendMessageW(
                control,
                CB_SETITEMHEIGHT,
                usize::MAX,
                scaled(27, dpi) as LPARAM,
            );
        } else if id == VALUES {
            SendMessageW(control, LB_SETITEMHEIGHT, 0, scaled(21, dpi) as LPARAM);
        }
    }
}

const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

pub struct Layout {
    window: HWND,
    instance: HINSTANCE,
    dpi: i32,
    dark: bool,
    section: Cell<Section>,
}

impl Layout {
    pub fn new(window: HWND, instance: HINSTANCE, dpi: i32, dark: bool) -> Self {
        Self {
            window,
            instance,
            dpi,
            dark,
            section: Cell::new(Section::Always),
        }
    }

    fn child(
        &self,
        class: windows_sys::core::PCWSTR,
        id: u16,
        text: &str,
        style: u32,
        bounds: Bounds,
    ) -> HWND {
        let (x, y, width, height) = layout_bounds(VIEW.get(), id, bounds);
        let s = |n| scaled(n, self.dpi);
        let text = wide(text);
        // SAFETY: strings are terminated; the parent owns the created child window.
        unsafe {
            let child = CreateWindowExW(
                0,
                class,
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                s(x),
                s(y),
                s(width),
                s(height),
                self.window,
                id as usize as HMENU,
                self.instance,
                null(),
            );
            apply_font(child, id);
            adjust_control(child, id, self.dpi);
            CONTROLS
                .with_borrow_mut(|controls| controls.push((child, id, bounds, self.section.get())));
            child
        }
    }

    pub fn section(&self, section: Section) {
        self.section.set(section);
    }

    pub fn label(&self, id: u16, text: &str, bounds: Bounds) {
        let ellipsis = if id == DETAIL { SS_ENDELLIPSIS } else { 0 };
        self.child(w!("STATIC"), id, text, SS_NOPREFIX | ellipsis, bounds);
    }

    pub fn button(&self, id: u16, text: &str, bounds: Bounds) -> HWND {
        self.child(
            w!("BUTTON"),
            id,
            text,
            WS_TABSTOP | BS_OWNERDRAW as u32,
            bounds,
        )
    }

    pub fn edit(&self, id: u16, text: &str, limit: usize, bounds: Bounds) {
        let kind = if id == SECRET { ES_PASSWORD } else { 0 };
        let child = self.child(
            w!("EDIT"),
            id,
            text,
            WS_TABSTOP | (ES_AUTOHSCROLL | kind) as u32,
            bounds,
        );
        // SAFETY: edit messages take integer limits and a temporarily valid rectangle.
        unsafe {
            SendMessageW(child, EM_SETLIMITTEXT, limit, 0);
        }
    }

    pub fn combo(&self, id: u16, options: &[&str], selected: usize, bounds: Bounds) {
        let child = self.child(
            w!("COMBOBOX"),
            id,
            "",
            WS_TABSTOP
                | WS_VSCROLL
                | (CBS_DROPDOWNLIST | CBS_OWNERDRAWFIXED | CBS_HASSTRINGS) as u32,
            bounds,
        );
        // SAFETY: combo box copies each null-terminated string synchronously.
        unsafe {
            if self.dark {
                theme_control(child, id, true);
            }
            for item in options {
                SendMessageW(child, CB_ADDSTRING, 0, wide(item).as_ptr() as LPARAM);
            }
            SendMessageW(child, CB_SETCURSEL, selected, 0);
        }
        controls::style(child, true);
    }

    pub fn check(&self, id: u16, text: &str, checked: bool, bounds: Bounds) {
        let child = self.child(
            w!("BUTTON"),
            id,
            text,
            WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            bounds,
        );
        // SAFETY: these messages operate on the live button created above.
        unsafe {
            if self.dark {
                theme_control(child, id, true);
            }
            SendMessageW(
                child,
                BM_SETCHECK,
                if checked { BST_CHECKED as WPARAM } else { 0 },
                0,
            );
        }
        controls::style(child, false);
    }

    pub fn header(&self, title: &str, subtitle: &str) {
        self.label(TITLE, title, (86, 17, 495, 39));
        self.label(SECONDARY, subtitle, (87, 53, 490, 21));
    }

    pub fn dashboard(&self, tr: impl Fn(&'static str, &'static str) -> &'static str) {
        self.header(
            "RustNUT",
            tr("Network UPS monitor", "Мониторинг сетевого ИБП"),
        );
        self.button(SETTINGS, tr("Settings", "Настройки"), (604, 26, 144, 39));
        self.label(STATUS, "", (0, 0, 0, 0));
        self.label(DETAIL, "", (0, 0, 0, 0));
        self.label(gauges::RUNTIME_TEXT, "", (0, 0, 0, 0));
        self.label(gauges::IDENTITY, "", (0, 0, 0, 0));
        for id in gauges::VALUE_IDS {
            self.child(w!("STATIC"), id, "", SS_OWNERDRAW, (0, 0, 0, 0));
        }
        self.label(SECTION, tr("UPS details", "Параметры ИБП"), (0, 0, 0, 0));
        self.label(gauges::TABLE_KEY, tr("Parameter", "Параметр"), (0, 0, 0, 0));
        self.label(gauges::TABLE_VALUE, tr("Value", "Значение"), (0, 0, 0, 0));
        let list = self.child(
            w!("LISTBOX"),
            VALUES,
            "",
            WS_TABSTOP
                | (LBS_NOINTEGRALHEIGHT | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS | LBS_NOTIFY) as u32,
            (38, 425, 700, 126),
        );
        let scroll = self.child(w!("STATIC"), scroll::BAR, "", SS_NOTIFY, (0, 0, 0, 0));
        scroll::style(list, false);
        scroll::style(scroll, true);
        self.button(
            CANCEL,
            tr("Cancel power action", "Отменить действие"),
            (532, 574, 216, 40),
        );
        self.label(SUMMARY, "", (29, 585, 492, 26));
    }

    pub fn field(&self, id: u16, title: &str, text: &str, limit: usize, bounds: Bounds) {
        let (x, y, width, height) = bounds;
        self.field_label(id, title, (x, y, width, 20));
        self.boxed_edit(id, text, limit, (x, y + 23, width, height));
    }

    pub fn field_label(&self, id: u16, title: &str, bounds: Bounds) {
        self.label(field_label_id(id), title, bounds);
    }

    pub fn boxed_edit(&self, id: u16, text: &str, limit: usize, bounds: Bounds) {
        let (x, y, width, height) = bounds;
        self.edit(id, text, limit, (x + 8, y + 5, width - 16, height - 10));
    }
}

pub fn child(window: HWND, id: u16) -> HWND {
    // SAFETY: the parent window remains alive throughout its UI dispatch.
    unsafe { GetDlgItem(window, id as i32) }
}

pub fn text(window: HWND, id: u16, value: &str) {
    let control = child(window, id);
    let mut short = [0u16; 256];
    // SAFETY: short has room for the measured text and its terminator. SetWindowText copies its input.
    unsafe {
        let len = GetWindowTextW(control, short.as_mut_ptr(), short.len() as i32).max(0) as usize;
        let unchanged = if len < short.len() - 1 {
            value.encode_utf16().eq(short[..len].iter().copied())
        } else {
            read(window, id) == value
        };
        if !unchanged {
            SetWindowTextW(control, wide(value).as_ptr());
        }
    }
}

pub fn read(window: HWND, id: u16) -> String {
    let control = child(window, id);
    // SAFETY: this buffer is sized for GetWindowTextW, including its terminator.
    unsafe {
        let mut data = vec![0; GetWindowTextLengthW(control).max(0) as usize + 1];
        let n = GetWindowTextW(control, data.as_mut_ptr(), data.len() as i32);
        String::from_utf16_lossy(&data[..n.max(0) as usize])
    }
}

pub fn password(window: HWND) -> zeroize::Zeroizing<String> {
    let control = child(window, SECRET);
    // SAFETY: GetWindowTextW writes no more than the supplied UTF-16 capacity.
    unsafe {
        let mut data = zeroize::Zeroizing::new(vec![0u16; 1025]);
        let n = GetWindowTextW(control, data.as_mut_ptr(), data.len() as i32);
        zeroize::Zeroizing::new(String::from_utf16_lossy(&data[..n.max(0) as usize]))
    }
}

pub fn cue(window: HWND, id: u16, placeholder: &str) {
    // SAFETY: the edit control copies the terminated cue text synchronously.
    unsafe {
        SendMessageW(
            child(window, id),
            EM_SETCUEBANNER,
            0,
            wide(placeholder).as_ptr() as LPARAM,
        )
    };
}

pub fn choice(window: HWND, id: u16) -> usize {
    // SAFETY: the message takes no pointers.
    unsafe { SendMessageW(child(window, id), CB_GETCURSEL, 0, 0).max(0) as usize }
}

pub fn combo_open(window: HWND) -> bool {
    // SAFETY: combo messages take no pointers.
    [TLS, DARK, RU, ACTION, FREQUENCY]
        .into_iter()
        .any(|id| unsafe {
            let combo = child(window, id);
            !combo.is_null() && SendMessageW(combo, CB_GETDROPPEDSTATE, 0, 0) != 0
        })
}

pub fn checked(window: HWND, id: u16) -> bool {
    // SAFETY: the message takes no pointers.
    unsafe { SendMessageW(child(window, id), BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

pub fn set_list<T>(
    window: HWND,
    rows: impl ExactSizeIterator<Item = T>,
    mut format_row: impl FnMut(T, &mut String),
) {
    let list = child(window, VALUES);
    let mut previous = TABLE_ROWS.take();
    let rebuild = previous.len() != rows.len();
    let mut formatted = String::new();
    // SAFETY: each row is copied by the native list and the UI thread restores redraw.
    unsafe {
        let pause_redraw = || {
            let top = SendMessageW(list, LB_GETTOPINDEX, 0, 0).max(0) as usize;
            let selected = SendMessageW(list, LB_GETCURSEL, 0, 0);
            SendMessageW(list, WM_SETREDRAW, 0, 0);
            (top, selected)
        };
        let mut position = None;
        if rebuild {
            position = Some(pause_redraw());
            previous.resize_with(rows.len(), String::new);
            SendMessageW(list, LB_RESETCONTENT, 0, 0);
        }
        for (index, (old, row)) in previous.iter_mut().zip(rows).enumerate() {
            formatted.clear();
            format_row(row, &mut formatted);
            if rebuild || *old != formatted {
                position.get_or_insert_with(pause_redraw);
                let text = wide(&formatted);
                if rebuild {
                    SendMessageW(list, LB_ADDSTRING, 0, text.as_ptr() as LPARAM);
                } else {
                    SendMessageW(list, LB_DELETESTRING, index, 0);
                    SendMessageW(list, LB_INSERTSTRING, index, text.as_ptr() as LPARAM);
                }
                std::mem::swap(old, &mut formatted);
            }
        }
        if let Some((top, selected)) = position {
            SendMessageW(list, LB_SETCURSEL, selected as WPARAM, 0);
            SendMessageW(list, LB_SETTOPINDEX, top, 0);
            SendMessageW(list, WM_SETREDRAW, 1, 0);
            InvalidateRect(list, null(), 1);
            scroll::invalidate(window);
        }
    }
    TABLE_ROWS.replace(previous);
}

pub fn clear(window: HWND) {
    TABLE_ROWS.with_borrow_mut(Vec::clear);
    CONTROLS.with_borrow_mut(Vec::clear);
    // SAFETY: iterating through direct children and destroying each is confined to the UI thread.
    unsafe {
        loop {
            let child = GetWindow(window, GW_CHILD);
            if child.is_null() || DestroyWindow(child) == 0 {
                break;
            }
        }
    }
}
