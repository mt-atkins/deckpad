//! Draws key faces: a rounded colour tile with a centred label.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgb, RgbImage};
use std::path::{Path, PathBuf};
use std::process::Command;

const BACKGROUND: [u8; 3] = [0, 0, 0];

pub struct Renderer {
    font: Option<FontVec>,
}

impl Renderer {
    pub fn new(configured: Option<&Path>) -> Renderer {
        let font = find_font(configured).and_then(|path| {
            let bytes = std::fs::read(&path).ok()?;
            FontVec::try_from_vec(bytes).ok()
        });
        if font.is_none() {
            eprintln!("deckpad: no usable font found; keys will show colours only (set `font` in the config)");
        }
        Renderer { font }
    }

    /// Renders one square key face of `size` pixels.
    pub fn face(&self, size: u32, label: &str, color: [u8; 3], pressed: bool) -> RgbImage {
        let (tile, ink) = if pressed {
            (mix(color, [255, 255, 255], 0.55), [20, 20, 24])
        } else {
            (mix(color, [0, 0, 0], 0.25), [255, 255, 255])
        };

        let mut img = RgbImage::from_pixel(size, size, Rgb(BACKGROUND));
        let s = size as f32;
        let (inset, radius) = (s * 0.04, s * 0.16);
        for y in 0..size {
            for x in 0..size {
                let cov = rounded_rect_coverage(x as f32 + 0.5, y as f32 + 0.5, s, inset, radius);
                if cov > 0.0 {
                    blend(&mut img, x as i32, y as i32, tile, cov);
                }
            }
        }

        self.draw_label(&mut img, label, ink);
        img
    }

    fn draw_label(&self, img: &mut RgbImage, label: &str, ink: [u8; 3]) {
        let lines: Vec<&str> = label.lines().filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            return;
        }
        let s = img.width() as f32;
        let max_width = s * 0.80;
        let mut px = s * match lines.len() {
            1 => 0.38,
            2 => 0.30,
            _ => 0.23,
        };
        if lines.len() == 1 && arrow(lines[0]).is_some() {
            px = s * 0.55;
        }

        let Some(font) = &self.font else {
            // Arrows need no font, so still draw those.
            let line_h = px * 1.2;
            let top = (s - line_h * lines.len() as f32) / 2.0;
            for (i, line) in lines.iter().enumerate() {
                if let Some(dir) = arrow(line) {
                    draw_arrow(img, dir, s / 2.0, top + line_h * (i as f32 + 0.5), px * 0.75, ink);
                }
            }
            return;
        };

        let width_at = |px: f32, line: &str| -> f32 {
            let sf = font.as_scaled(PxScale::from(px));
            let mut w = 0.0;
            let mut prev = None;
            for ch in line.chars() {
                let id = font.glyph_id(ch);
                if let Some(p) = prev {
                    w += sf.kern(p, id);
                }
                w += sf.h_advance(id);
                prev = Some(id);
            }
            w
        };
        while px > 8.0 && lines.iter().any(|l| arrow(l).is_none() && width_at(px, l) > max_width) {
            px -= 1.0;
        }

        let sf = font.as_scaled(PxScale::from(px));
        let line_h = (sf.ascent() - sf.descent()) * 1.02;
        let top = (s - line_h * lines.len() as f32) / 2.0;
        for (i, line) in lines.iter().enumerate() {
            let line_top = top + line_h * i as f32;
            if let Some(dir) = arrow(line) {
                draw_arrow(img, dir, s / 2.0, line_top + line_h / 2.0, px * 0.75, ink);
                continue;
            }
            let mut x = (s - width_at(px, line)) / 2.0;
            let baseline = line_top + sf.ascent();
            let mut prev = None;
            for ch in line.chars() {
                let id = font.glyph_id(ch);
                if let Some(p) = prev {
                    x += sf.kern(p, id);
                }
                let glyph = id.with_scale_and_position(px, point(x, baseline));
                if let Some(outline) = font.outline_glyph(glyph) {
                    let bounds = outline.px_bounds();
                    outline.draw(|gx, gy, c| {
                        blend(img, bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, ink, c);
                    });
                }
                x += sf.h_advance(id);
                prev = Some(id);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// A line consisting of a single arrow character is drawn as a triangle,
/// since most UI fonts have no arrow glyphs.
fn arrow(line: &str) -> Option<Dir> {
    match line.trim() {
        "↑" | "^" => Some(Dir::Up),
        "↓" => Some(Dir::Down),
        "←" => Some(Dir::Left),
        "→" => Some(Dir::Right),
        _ => None,
    }
}

fn draw_arrow(img: &mut RgbImage, dir: Dir, cx: f32, cy: f32, size: f32, ink: [u8; 3]) {
    let h = size / 2.0;
    let pts = match dir {
        Dir::Up => [(cx, cy - h), (cx - h, cy + h * 0.8), (cx + h, cy + h * 0.8)],
        Dir::Down => [(cx, cy + h), (cx + h, cy - h * 0.8), (cx - h, cy - h * 0.8)],
        Dir::Left => [(cx - h, cy), (cx + h * 0.8, cy - h), (cx + h * 0.8, cy + h)],
        Dir::Right => [(cx + h, cy), (cx - h * 0.8, cy + h), (cx - h * 0.8, cy - h)],
    };
    let edge = |a: (f32, f32), b: (f32, f32), p: (f32, f32)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    let inside = |p: (f32, f32)| {
        let (d0, d1, d2) = (edge(pts[0], pts[1], p), edge(pts[1], pts[2], p), edge(pts[2], pts[0], p));
        (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
    };
    let (x0, x1) = ((cx - h).floor() as i32, (cx + h).ceil() as i32);
    let (y0, y1) = ((cy - h).floor() as i32, (cy + h).ceil() as i32);
    for y in y0..=y1 {
        for x in x0..=x1 {
            // 4x4 supersampling for smooth edges.
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    if inside((x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0)) {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                blend(img, x, y, ink, hits as f32 / 16.0);
            }
        }
    }
}

fn rounded_rect_coverage(px: f32, py: f32, size: f32, inset: f32, radius: f32) -> f32 {
    let half = size / 2.0 - inset;
    let qx = (px - size / 2.0).abs() - (half - radius);
    let qy = (py - size / 2.0).abs() - (half - radius);
    let dist = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
    (0.5 - dist).clamp(0.0, 1.0)
}

fn blend(img: &mut RgbImage, x: i32, y: i32, color: [u8; 3], alpha: f32) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return;
    }
    let px = img.get_pixel_mut(x as u32, y as u32);
    px.0 = mix(px.0, color, alpha.clamp(0.0, 1.0));
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let f = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

fn find_font(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        eprintln!("deckpad: font {} not found, falling back to a system font", p.display());
    }
    if let Ok(out) = Command::new("fc-match").args(["-f", "%{file}", "sans:bold"]).output() {
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let supported = matches!(path.extension().and_then(|e| e.to_str()), Some("ttf" | "otf"));
        if out.status.success() && supported && path.exists() {
            return Some(path);
        }
    }
    [
        "/usr/share/fonts/google-noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/liberation-sans/LiberationSans-Bold.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}
