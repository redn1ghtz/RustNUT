use super::*;
use windows_sys::Win32::UI::{
    Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
    Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
};

pub const BAR: u16 = 933;

struct Scroll {
    region: RECT,
    top: i32,
    maximum: i32,
    thumb: i32,
}

thread_local! {
    static DRAG: Cell<Option<i32>> = const { Cell::new(None) };
    static WHEEL: Cell<i32> = const { Cell::new(0) };
}

impl Scroll {
    fn new(bar: HWND) -> Self {
        let dpi = VIEW.get().dpi;
        let mut rect = RECT::default();
        // SAFETY: bar and its sibling list have the same live parent.
        unsafe {
            GetClientRect(bar, &mut rect);
            let list = child(GetParent(bar), VALUES);
            let count = SendMessageW(list, LB_GETCOUNT, 0, 0).max(0) as i32;
            let top = SendMessageW(list, LB_GETTOPINDEX, 0, 0).max(0) as i32;
            let mut viewport = RECT::default();
            GetClientRect(list, &mut viewport);
            let page = (viewport.bottom / scaled(21, dpi).max(1)).max(1);
            let inset = scaled(3, dpi);
            let region = super::rect(inset, inset, rect.right - inset, rect.bottom - inset);
            let length = (region.bottom - region.top).max(0);
            let thumb = (length * page / count.max(1))
                .max(scaled(25, dpi))
                .min(length);
            Self {
                region,
                top,
                maximum: (count - page).max(0),
                thumb,
            }
        }
    }

    fn thumb_top(&self) -> i32 {
        let distance = self.region.bottom - self.region.top - self.thumb;
        self.region.top + distance * self.top / self.maximum.max(1)
    }

    fn index(&self, pixel: i32) -> i32 {
        let space = (self.region.bottom - self.region.top - self.thumb).max(1);
        ((pixel - self.region.top).clamp(0, space) * self.maximum + space / 2) / space
    }
}

pub fn style(control: HWND, bar: bool) {
    // SAFETY: both subclassed controls are owned by the UI thread.
    unsafe {
        SetWindowSubclass(control, Some(dispatch), 1, usize::from(bar));
    }
}

fn draw(bar: HWND, dc: HDC) {
    let scroll = Scroll::new(bar);
    THEME.with_borrow(|theme| {
        let Some(t) = theme else { return };
        let dpi = t.dpi;
        // SAFETY: the supplied DC is valid during paint.
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(bar, &mut rect);
            FillRect(dc, &rect, t.panel);
        }
        if scroll.maximum > 0 {
            let track = blend(t.border, t.panel_color, 62);
            rounded(dc, scroll.region, scaled(10, dpi), track, track);
            let origin = scroll.thumb_top();
            let thumb = blend(t.accent, t.text, 36);
            let region = rect(
                scroll.region.left,
                origin,
                scroll.region.right,
                origin + scroll.thumb,
            );
            rounded(dc, region, scaled(9, dpi), thumb, thumb);
        }
    });
}

unsafe extern "system" fn dispatch(
    window: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    subclass: usize,
    bar: usize,
) -> LRESULT {
    // SAFETY: these messages and sibling handles stay on the control's own thread.
    unsafe {
        let parent = GetParent(window);
        if msg == WM_NCDESTROY {
            RemoveWindowSubclass(window, Some(dispatch), subclass);
        }
        if bar != 0 {
            match msg {
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(window, &mut paint);
                    draw(window, dc);
                    EndPaint(window, &paint);
                    return 0;
                }
                WM_PRINTCLIENT => {
                    draw(window, wp as HDC);
                    return 0;
                }
                WM_LBUTTONDOWN => {
                    let scroll = Scroll::new(window);
                    if scroll.maximum > 0 {
                        let y = (lp >> 16) as i16 as i32;
                        let top = scroll.thumb_top();
                        let offset = if (top..top + scroll.thumb).contains(&y) {
                            y - top
                        } else {
                            scroll.thumb / 2
                        };
                        DRAG.set(Some(offset));
                        SetCapture(window);
                        let list = child(parent, VALUES);
                        SendMessageW(list, LB_SETTOPINDEX, scroll.index(y - offset) as WPARAM, 0);
                    }
                    return 0;
                }
                WM_MOUSEMOVE if DRAG.get().is_some() => {
                    let y = (lp >> 16) as i16 as i32;
                    let scroll = Scroll::new(window);
                    let list = child(parent, VALUES);
                    SendMessageW(
                        list,
                        LB_SETTOPINDEX,
                        scroll.index(y - DRAG.get().unwrap()) as WPARAM,
                        0,
                    );
                    return 0;
                }
                WM_LBUTTONUP if DRAG.take().is_some() => {
                    ReleaseCapture();
                    return 0;
                }
                WM_CAPTURECHANGED => DRAG.set(None),
                _ => {}
            }
        } else if msg == WM_MOUSEWHEEL {
            let delta = (wp >> 16) as i16 as i32 + WHEEL.get();
            WHEEL.set(delta % WHEEL_DELTA as i32);
            let scroll = Scroll::new(child(parent, BAR));
            let next = (scroll.top - delta / WHEEL_DELTA as i32 * 3).clamp(0, scroll.maximum);
            SendMessageW(window, LB_SETTOPINDEX, next as WPARAM, 0);
            return 0;
        }
        let result = DefSubclassProc(window, msg, wp, lp);
        if bar == 0
            && matches!(
                msg,
                WM_SIZE | WM_KEYDOWN | WM_LBUTTONDOWN | LB_SETTOPINDEX | LB_RESETCONTENT
            )
        {
            invalidate(parent);
        }
        result
    }
}

pub fn invalidate(parent: HWND) {
    // SAFETY: the bar is an optional child of the same parent.
    unsafe {
        let bar = child(parent, BAR);
        if !bar.is_null() {
            InvalidateRect(bar, null(), 0);
        }
    }
}
