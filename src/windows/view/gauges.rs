use super::*;

pub const COUNT: usize = 6;
pub const RUNTIME_TEXT: u16 = 929;
pub const TABLE_KEY: u16 = 930;
pub const TABLE_VALUE: u16 = 931;
pub const IDENTITY: u16 = 934;
pub const VALUE_IDS: [u16; COUNT] = [122, 123, 121, BATTERY, 120, 124];

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Meter {
    pub reading: Option<f64>,
    pub low: f64,
    pub high: f64,
    pub flagged: bool,
}

pub fn card(v: Viewport, index: usize) -> RECT {
    let columns = if v.width >= 1120 { 6 } else { 3 };
    let area = v.width - 56;
    let column = index as i32 % columns;
    let row = index as i32 / columns;
    let gap = 12;
    let width = (area - gap * (columns - 1)) / columns;
    let remainder = area - width * columns - gap * (columns - 1);
    let left = 28 + column * (width + gap) + remainder * column / (columns - 1);
    let top = 193 + row * 145;
    rect(left, top, left + width, top + 133)
}

pub fn table(v: Viewport) -> RECT {
    let last = card(v, COUNT - 1);
    rect(28, last.bottom + 45, v.width - 28, v.height - 68)
}

pub fn hero(v: Viewport) -> RECT {
    rect(28, 88, v.width - 28, 181)
}

pub fn split(width: i32) -> i32 {
    width * 42 / 100
}

pub fn position(v: Viewport, id: u16, fallback: Bounds) -> Bounds {
    if let Some(index) = VALUE_IDS.iter().position(|&value| value == id) {
        let cell = card(v, index);
        return (
            cell.left,
            cell.top,
            cell.right - cell.left,
            cell.bottom - cell.top,
        );
    }
    let table = table(v);
    let width = table.right - table.left - 40;
    match id {
        STATUS => (51, 95, (v.width * 45 / 100).min(520), 32),
        DETAIL => (50, 127, v.width - 111, 23),
        IDENTITY => (50, 151, v.width - 111, 23),
        RUNTIME_TEXT => (v.width - 285, 101, 231, 24),
        SECTION => (29, table.top - 34, 400, 29),
        TABLE_KEY => (50, table.top + 10, split(width) - 16, 23),
        TABLE_VALUE => (
            50 + split(width),
            table.top + 10,
            width - split(width) - 12,
            23,
        ),
        VALUES => (
            38,
            table.top + 36,
            width,
            (table.bottom - table.top - 45).max(30),
        ),
        super::scroll::BAR => (
            table.right - 29,
            table.top + 36,
            18,
            (table.bottom - table.top - 45).max(30),
        ),
        CANCEL => (v.width - 244, v.height - 55, 216, 39),
        SUMMARY => (29, v.height - 47, v.width - 278, 27),
        SETTINGS => (v.width - 174, 26, 146, 39),
        _ => fallback,
    }
}

pub(super) fn paint(draw: &DRAWITEMSTRUCT, theme: &Theme, meter: Meter) {
    let dc = draw.hDC;
    let bounds = draw.rcItem;
    let s = |n| scaled(n, theme.dpi);
    // SAFETY: Windows owns the DC and live static control for the entire draw message.
    unsafe { FillRect(dc, &bounds, theme.background) };
    rounded(dc, bounds, s(22), theme.panel_color, theme.border);
    let center = (bounds.left + bounds.right) / 2;
    let y = bounds.top + s(77);
    let radius = s(43);
    let circle = rect(center - radius, y - radius, center + radius, y + radius);
    let sweep = meter.reading.map(|value| {
        let amount = ((value - meter.low) / (meter.high - meter.low)).clamp(0.0, 1.0) as f32;
        (
            amount,
            if meter.flagged {
                theme.warning
            } else {
                theme.accent
            },
        )
    });
    shapes::dial(dc, circle, s(5).max(2) as f32, theme.border, sweep);

    let mut text = [0u16; 230];
    // SAFETY: Win32 writes at most the supplied buffer capacity.
    let len = unsafe { GetWindowTextW(draw.hwndItem, text.as_mut_ptr(), text.len() as i32) };
    let mut lines = text[..len.max(0) as usize].split(|&ch| ch == 10);
    let caption = lines.next().unwrap_or_default();
    let value = lines.next().unwrap_or_default();
    let note = lines.next().unwrap_or_default();
    let mut title = rect(
        bounds.left + s(8),
        bounds.top + s(7),
        bounds.right - s(8),
        bounds.top + s(30),
    );
    draw_text(
        dc,
        &mut title,
        caption,
        theme.regular,
        theme.text,
        DT_CENTER,
    );
    let unavailable = meter.reading.is_none() && value.len() > 2;
    let center_value = if unavailable { note } else { value };
    if !center_value.is_empty() {
        let mut reading = rect(center - s(42), y - s(13), center + s(42), y + s(13));
        let font = if meter.reading.is_some() {
            theme.heading
        } else {
            theme.regular
        };
        draw_text(dc, &mut reading, center_value, font, theme.text, DT_CENTER);
    }
    let note = if unavailable { value } else { note };
    if !note.is_empty() {
        let mut foot = rect(center - s(45), y + s(27), center + s(45), y + s(48));
        draw_text(dc, &mut foot, note, theme.regular, theme.muted, DT_CENTER);
    }
    if meter.high > meter.low {
        for (limit, left) in [(meter.low, true), (meter.high, false)] {
            let mut area = if left {
                rect(bounds.left + s(7), y + s(27), center - s(49), y + s(48))
            } else {
                rect(center + s(49), y + s(27), bounds.right - s(7), y + s(48))
            };
            let number = format!("{limit:.1}");
            let text = wide(number.trim_end_matches(".0"));
            draw_text(
                dc,
                &mut area,
                &text[..text.len() - 1],
                theme.regular,
                theme.muted,
                if left { DT_RIGHT } else { DT_LEFT },
            );
        }
    }
}
