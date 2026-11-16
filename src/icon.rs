const BRAND_TILE: u32 = 0x2b1e1a;
const BRAND_BOLT: u32 = 0xff6b4a;

const BOLT: [(f32, f32); 6] = [
    (0.57, 0.17),
    (0.23, 0.55),
    (0.47, 0.55),
    (0.42, 0.83),
    (0.77, 0.43),
    (0.54, 0.43),
];

fn in_bolt(x: f32, y: f32) -> bool {
    BOLT.iter()
        .zip(BOLT.iter().cycle().skip(1))
        .fold(false, |inside, (&(ax, ay), &(bx, by))| {
            inside ^ ((ay > y) != (by > y) && x < ax + (bx - ax) * (y - ay) / (by - ay))
        })
}

fn in_tile(x: f32, y: f32) -> bool {
    let dx = ((x - 0.5).abs() - 0.25).max(0.0);
    let dy = ((y - 0.5).abs() - 0.25).max(0.0);
    dx * dx + dy * dy <= 0.16 * 0.16
}

pub fn pixels(size: usize) -> Vec<u8> {
    const SAMPLES: usize = 8;
    const TOTAL: u32 = (SAMPLES * SAMPLES) as u32;
    let background = BRAND_TILE.to_be_bytes();
    let foreground = BRAND_BOLT.to_be_bytes();
    let mut image = Vec::with_capacity(size * size * 4);
    let step = 1.0 / (size * SAMPLES) as f32;
    for y in 0..size {
        for x in 0..size {
            let mut tile = 0u32;
            let mut bolt = 0u32;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let px = ((x * SAMPLES + sx) as f32 + 0.5) * step;
                    let py = ((y * SAMPLES + sy) as f32 + 0.5) * step;
                    if in_tile(px, py) {
                        tile += 1;
                        bolt += u32::from(in_bolt(px, py));
                    }
                }
            }
            let color = |index: usize| {
                let bolt_color = u32::from(foreground[index]) * bolt;
                let tile_color = u32::from(background[index]) * (tile - bolt);
                ((bolt_color + tile_color + tile / 2) / tile.max(1)) as u8
            };
            image.extend([
                color(3),
                color(2),
                color(1),
                ((tile * 255 + TOTAL / 2) / TOTAL) as u8,
            ]);
        }
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_smooth_alpha_and_bolt_at_small_and_large_sizes() {
        for size in [16, 48, 128, 256] {
            let img = pixels(size);
            assert_eq!(img.len(), size * size * 4);
            assert_eq!(&img[..4], &[0, 0, 0, 0]);
            assert!(img.chunks_exact(4).any(|p| (1..255).contains(&p[3])));
            assert!(img.chunks_exact(4).any(|p| p == [0x1a, 0x1e, 0x2b, 255]));
            assert!(img.chunks_exact(4).any(|p| p == [0x4a, 0x6b, 0xff, 255]));
        }
    }
}
