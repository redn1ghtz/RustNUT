use super::*;
use windows_sys::Win32::{
    Foundation::POINT,
    UI::{
        Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
    },
};

pub fn style(control: HWND, combo: bool) {
    // SAFETY: the subclass lives for the duration of the native control on the UI thread.
    unsafe {
        SetWindowSubclass(control, Some(render), 1, usize::from(combo));
    }
}

unsafe extern "system" fn render(
    window: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    subclass: usize,
    combo: usize,
) -> LRESULT {
    // SAFETY: Windows dispatches these messages on the control's owning thread.
    unsafe {
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                draw(window, dc, combo != 0);
                EndPaint(window, &paint);
                return 0;
            }
            WM_PRINTCLIENT => {
                draw(window, wp as HDC, combo != 0);
                return 0;
            }
            WM_NCPAINT if combo != 0 => return 0,
            WM_NCDESTROY => {
                RemoveWindowSubclass(window, Some(render), subclass);
            }
            _ => {}
        }
        let result = DefSubclassProc(window, msg, wp, lp);
        if matches!(
            msg,
            WM_ENABLE
                | WM_SETFOCUS
                | WM_KILLFOCUS
                | WM_KEYDOWN
                | WM_KEYUP
                | WM_INPUTLANGCHANGE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | CB_SETCURSEL
                | BM_SETCHECK
                | WM_SETTEXT
                | CB_SHOWDROPDOWN
        ) {
            InvalidateRect(window, null(), 0);
        }
        result
    }
}

fn draw(window: HWND, dc: HDC, combo: bool) {
    THEME.with_borrow(|theme| {
        let Some(t) = theme else { return };
        let s = |n| scaled(n, t.dpi);
        // SAFETY: the DC and control remain live during this synchronous paint operation.
        unsafe {
            let disabled = IsWindowEnabled(window) == 0;
            let focus = GetFocus() == window;
            let text = if disabled {
                blend(t.muted, t.panel_color, 44)
            } else {
                t.text
            };
            let stroke = if focus {
                t.text
            } else if disabled {
                blend(t.border, t.panel_color, 52)
            } else {
                t.border
            };
            let mut area = RECT::default();
            GetClientRect(window, &mut area);
            FillRect(dc, &area, t.panel);
            let mut buffer = [0u16; 180];
            if combo {
                let fill = if disabled {
                    t.panel_color
                } else {
                    t.field_color
                };
                rounded(dc, area, s(12), fill, stroke);
                let index = SendMessageW(window, CB_GETCURSEL, 0, 0);
                let len = if index < 0 {
                    0
                } else {
                    SendMessageW(window, CB_GETLBTEXTLEN, index as usize, 0)
                };
                if (1..180).contains(&len) {
                    SendMessageW(
                        window,
                        CB_GETLBTEXT,
                        index as usize,
                        buffer.as_mut_ptr() as LPARAM,
                    );
                    let mut label = area;
                    label.left += s(11);
                    label.right -= s(30);
                    draw_text(
                        dc,
                        &mut label,
                        &buffer[..len as usize],
                        t.regular,
                        text,
                        DT_LEFT,
                    );
                }
                let x = area.right - s(16);
                let y = (area.top + area.bottom) / 2;
                lines(
                    dc,
                    &[(x - s(4), y - s(2)), (x, y + s(2)), (x + s(4), y - s(2))],
                    s(2),
                    text,
                );
            } else {
                let checked = SendMessageW(window, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
                let y = (area.bottom - s(18)) / 2;
                let box_rect = rect(s(2), y, s(20), y + s(18));
                let mut fill = if checked { t.accent } else { t.field_color };
                if disabled {
                    fill = blend(fill, t.panel_color, 65);
                }
                rounded(
                    dc,
                    box_rect,
                    s(7),
                    fill,
                    if checked && !focus { fill } else { stroke },
                );
                if checked {
                    lines(
                        dc,
                        &[(s(6), y + s(9)), (s(10), y + s(13)), (s(16), y + s(5))],
                        s(2),
                        if disabled { text } else { t.on_accent },
                    );
                }
                let len = GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32);
                let mut label = area;
                label.left += s(28);
                label.right -= s(2);
                draw_text(
                    dc,
                    &mut label,
                    &buffer[..len.max(0) as usize],
                    t.regular,
                    text,
                    DT_LEFT,
                );
                if focus {
                    DrawFocusRect(dc, &label);
                }
            }
        }
    });
}

fn lines(dc: HDC, coordinates: &[(i32, i32); 3], width: i32, color: COLORREF) {
    let points = coordinates.map(|(x, y)| POINT { x, y });
    // SAFETY: GDI reads exactly three points and we restore the original pen.
    unsafe {
        let pen = CreatePen(PS_SOLID, width, color);
        if pen.is_null() {
            return;
        }
        let old = SelectObject(dc, pen);
        Polyline(dc, points.as_ptr(), points.len() as i32);
        SelectObject(dc, old);
        DeleteObject(pen);
    }
}
