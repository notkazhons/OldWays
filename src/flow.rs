//! Slow, irregular color flow drawn behind the widget, tinted by the album art.
//!
//! A handful of soft, rotated ellipses drift over a dark (or light) base. The field is computed
//! on a coarse grid and smoothed up to the output buffer, so a frame costs well under a
//! millisecond. The output is a small top-down 32-bit DIB that the caller stretches to the window.

/// Ellipse blobs: (x phase, y phase, angle phase, x speed, y speed, spin speed, palette slot,
/// long radius, short radius). Radii are in window heights; speeds in radians per second.
const BLOBS: [(f32, f32, f32, f32, f32, f32, usize, f32, f32); 4] = [
    (0.0, 1.3, 0.4, 0.21, 0.33, 0.17, 0, 1.00, 0.34),
    (2.1, 0.2, 1.9, 0.27, 0.19, -0.23, 1, 0.80, 0.30),
    (4.0, 3.1, 3.3, -0.17, 0.29, 0.13, 2, 0.90, 0.28),
    (5.2, 4.4, 5.0, 0.13, -0.24, -0.19, 0, 0.70, 0.26),
];

/// Coarse grid is this many output pixels per cell.
const CELL: usize = 4;

pub type Rgb = [f32; 3];

/// Fill `out` (`w` x `h`, 0x00RRGGBB) with the flow at time `t` seconds. `base` is the window
/// background, `pal` the three art colors, `strength` how far the colors pull away from the base.
pub fn render(out: &mut Vec<u32>, w: usize, h: usize, t: f32, base: Rgb, pal: &[Rgb; 3], strength: f32) {
    out.resize(w * h, 0);
    if w < 2 || h < 2 {
        return;
    }
    let (gw, gh) = (w / CELL + 2, h / CELL + 2);
    let aspect = w as f32 / h as f32;

    // Blob centers and orientation for this instant.
    let mut blobs = [(0f32, 0f32, 0f32, 0f32, 0f32, 0f32, 0usize); BLOBS.len()];
    for (b, &(px, py, pa, sx, sy, sa, slot, la, sb)) in blobs.iter_mut().zip(BLOBS.iter()) {
        let cx = aspect * (0.5 + 0.55 * (t * sx + px).sin());
        let cy = 0.5 + 0.6 * (t * sy + py).sin();
        let ang = t * sa + pa;
        *b = (cx, cy, ang.cos(), ang.sin(), 1.0 / (la * la), 1.0 / (sb * sb), slot);
    }

    // Field on the coarse grid.
    let mut grid = vec![[0f32; 3]; gw * gh];
    for gy in 0..gh {
        let y = gy as f32 * CELL as f32 / h as f32;
        for gx in 0..gw {
            let x = gx as f32 * CELL as f32 / h as f32;
            let (mut sum, mut col) = (0f32, [0f32; 3]);
            for &(cx, cy, c, s, inv_a2, inv_b2, slot) in blobs.iter() {
                let (dx, dy) = (x - cx, y - cy);
                let (u, v) = (dx * c + dy * s, -dx * s + dy * c);
                let f = (-(u * u * inv_a2 + v * v * inv_b2)).exp();
                sum += f;
                for k in 0..3 {
                    col[k] += f * pal[slot][k];
                }
            }
            let amt = sum.min(1.0) * strength;
            let cell = &mut grid[gy * gw + gx];
            for k in 0..3 {
                let avg = if sum > 1e-4 { col[k] / sum } else { base[k] };
                cell[k] = base[k] + (avg - base[k]) * amt;
            }
        }
    }

    // Bilinear up to the output, with a little noise so the dark ramps do not band.
    let inv = 1.0 / CELL as f32;
    for y in 0..h {
        let fy = y as f32 * inv;
        let (y0, ty) = (fy as usize, fy.fract());
        for x in 0..w {
            let fx = x as f32 * inv;
            let (x0, tx) = (fx as usize, fx.fract());
            let (a, b) = (&grid[y0 * gw + x0], &grid[y0 * gw + x0 + 1]);
            let (c, d) = (&grid[(y0 + 1) * gw + x0], &grid[(y0 + 1) * gw + x0 + 1]);
            let n = noise(x as u32, y as u32);
            let mut px = 0u32;
            for k in 0..3 {
                let top = a[k] + (b[k] - a[k]) * tx;
                let bot = c[k] + (d[k] - c[k]) * tx;
                let v = (top + (bot - top) * ty + n).clamp(0.0, 255.0);
                px = (px << 8) | v as u32;
            }
            out[y * w + x] = px;
        }
    }
}

/// Cheap hash noise in about -1.5..1.5.
fn noise(x: u32, y: u32) -> f32 {
    let mut n = x.wrapping_mul(374761393) ^ y.wrapping_mul(668265263);
    n = (n ^ (n >> 13)).wrapping_mul(1274126177);
    ((n >> 24) as f32 / 255.0 - 0.5) * 3.0
}
