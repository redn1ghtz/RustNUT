use std::{cell::Cell, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{COLORREF, RECT},
    Graphics::{Gdi::HDC, GdiPlus::*},
};

thread_local! { static READY: Cell<bool> = const { Cell::new(false) }; }

pub struct Session(usize);

impl Session {
    pub fn start() -> Self {
        let mut token = 0;
        let options = GdiplusStartupInput {
            GdiplusVersion: 1,
            SuppressExternalCodecs: 1,
            ..Default::default()
        };
        // SAFETY: GDI+ owns the token until this session drops on the UI thread.
        let initialized = unsafe { GdiplusStartup(&mut token, &options, null_mut()) == Ok };
        READY.set(initialized);
        Self(if initialized { token } else { 0 })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.0 != 0 {
            READY.set(false);
            // SAFETY: the token was acquired on this thread and all drawing has completed.
            unsafe { GdiplusShutdown(self.0) };
        }
    }
}

struct Canvas(*mut GpGraphics);

impl Canvas {
    fn new(dc: HDC) -> Option<Self> {
        if !READY.get() || dc.is_null() {
            return None;
        }
        let mut graphics = null_mut();
        // SAFETY: the caller keeps the underlying DC valid for the entire operation.
        unsafe {
            if GdipCreateFromHDC(dc, &mut graphics) != Ok {
                return None;
            }
            GdipSetSmoothingMode(graphics, SmoothingModeAntiAlias8x8);
        }
        Some(Self(graphics))
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: this Canvas owns the GDI+ graphics object.
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

fn argb(color: COLORREF) -> u32 {
    0xff000000 | ((color & 255) << 16) | (color & 0x00ff00) | ((color >> 16) & 255)
}

pub fn rounded(dc: HDC, rect: RECT, radius: i32, fill: COLORREF, edge: COLORREF) -> bool {
    let Some(canvas) = Canvas::new(dc) else {
        return false;
    };
    let width = (rect.right - rect.left) as f32;
    let height = (rect.bottom - rect.top) as f32;
    if width < 2.0 || height < 2.0 {
        return true;
    }
    let x = rect.left as f32 + 0.5;
    let y = rect.top as f32 + 0.5;
    let width = width - 1.0;
    let height = height - 1.0;
    let corner = (radius as f32).clamp(1.0, width.min(height));
    // SAFETY: path, pen and brush are temporary and deleted after use.
    unsafe {
        let mut path = null_mut();
        if GdipCreatePath(FillModeAlternate, &mut path) != Ok {
            return false;
        }
        for (dx, dy, angle) in [
            (0.0, 0.0, 180.0),
            (width - corner, 0.0, 270.0),
            (width - corner, height - corner, 0.0),
            (0.0, height - corner, 90.0),
        ] {
            GdipAddPathArc(path, x + dx, y + dy, corner, corner, angle, 90.0);
        }
        GdipClosePathFigure(path);
        let mut brush = null_mut();
        let filled = GdipCreateSolidFill(argb(fill), &mut brush) == Ok;
        if filled {
            GdipFillPath(canvas.0, brush.cast(), path);
            GdipDeleteBrush(brush.cast());
        }
        if fill != edge {
            let mut pen = null_mut();
            if GdipCreatePen1(argb(edge), 1.0, UnitPixel, &mut pen) == Ok {
                GdipDrawPath(canvas.0, pen, path);
                GdipDeletePen(pen);
            }
        }
        GdipDeletePath(path);
        filled
    }
}

pub fn dial(
    dc: HDC,
    circle: RECT,
    width: f32,
    track: COLORREF,
    value: Option<(f32, COLORREF)>,
) -> bool {
    let Some(canvas) = Canvas::new(dc) else {
        return false;
    };
    let draw = |ratio, color| {
        let mut pen = null_mut();
        // SAFETY: the pen is scoped to this draw call and the DC remains live.
        unsafe {
            if GdipCreatePen1(argb(color), width, UnitPixel, &mut pen) == Ok {
                GdipSetPenStartCap(pen, LineCapRound);
                GdipSetPenEndCap(pen, LineCapRound);
                GdipDrawArc(
                    canvas.0,
                    pen,
                    circle.left as f32,
                    circle.top as f32,
                    (circle.right - circle.left) as f32,
                    (circle.bottom - circle.top) as f32,
                    147.6,
                    244.8 * ratio,
                );
                GdipDeletePen(pen);
            }
        }
    };
    draw(1.0, track);
    if let Some((ratio, color)) = value {
        if ratio > 0.0 {
            draw(ratio, color);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{
        BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
        DeleteDC, DeleteObject, SelectObject,
    };

    #[test]
    fn rounded_corners_blend_at_multiple_dpi_scales() {
        let _session = Session::start();
        const SIDE: usize = 128;
        // SAFETY: the top-down DIB allocates exactly SIDE*SIDE pixels at four bytes each.
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            assert!(!dc.is_null());
            let mut pixels = null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: SIDE as i32,
                    biHeight: -(SIDE as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            assert!(!bitmap.is_null() && !pixels.is_null());
            let before = SelectObject(dc, bitmap);
            for dpi in [96, 144, 192] {
                std::ptr::write_bytes(pixels.cast::<u8>(), 255, SIDE * SIDE * 4);
                let scale = |n| super::super::scaled(n, dpi);
                assert!(rounded(
                    dc,
                    RECT {
                        left: scale(8),
                        top: scale(8),
                        right: scale(56),
                        bottom: scale(49),
                    },
                    scale(18),
                    0,
                    0
                ));
                let data = std::slice::from_raw_parts(pixels.cast::<u8>(), SIDE * SIDE * 4);
                assert!(
                    data.chunks_exact(4)
                        .any(|pixel| (1..255).contains(&pixel[0]))
                );
                assert_eq!(data[0], 255);
            }
            SelectObject(dc, before);
            DeleteObject(bitmap);
            DeleteDC(dc);
        }
    }
}
