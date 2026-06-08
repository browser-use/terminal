//! Centered "Grok-style" welcome screen: animated 3D braille BU logo + menu.
//! Port of mockup A from the HTML compare page.

use std::time::Instant;

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::{bold, muted, text_style};

// ─────────────────────────── Anim state ───────────────────────────
pub struct WelcomeAnim {
    pub rx: f32,
    pub ry: f32,
    pub vx: f32,
    pub vy: f32,
    pub base_rx: f32,
    pub target_vy: f32,
    pub last_tick: Instant,
    rng: u32,
}

impl WelcomeAnim {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(1)
            | 1;
        // Start at the canonical Browser Use orbit-mark orientation
        // (no global rotation applied — the ring `base_a`/`base_b`
        // already carry the right tilt/roll), then let the gentle
        // y-axis drift take over.
        Self {
            rx: 0.0,
            ry: 0.0,
            vx: 0.0,
            vy: 0.0,
            base_rx: 0.0,
            target_vy: 0.4,
            last_tick: Instant::now(),
            rng: seed,
        }
    }

    fn rand(&mut self) -> f32 {
        // xorshift32
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x as f32) / (u32::MAX as f32)
    }

    /// Advance the animation; call ~14fps from the event loop.
    pub fn tick(&mut self) {
        let dt = self.last_tick.elapsed().as_secs_f32().min(0.1);
        self.last_tick = Instant::now();
        self.rx += self.vx * dt;
        self.ry += self.vy * dt;
        let decay = 0.5_f32.powf(dt / 1.0);
        self.vx *= decay;
        self.vy = self.vy * decay + self.target_vy * (1.0 - decay);
        // gentle spring back to the resting tilt so post-click rx returns home
        self.rx += (self.base_rx - self.rx) * (1.0 - (-dt * 1.2_f32).exp());
    }

    pub fn throw(&mut self) {
        let rx_imp = (self.rand() * 2.0 - 1.0) * 9.0 + 5.0;
        let ry_imp = (self.rand() * 2.0 - 1.0) * 7.0 + 7.0;
        self.vx += rx_imp;
        self.vy += ry_imp;
    }
}

impl Default for WelcomeAnim {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────── Geometry ───────────────────────────
const RING_SAMPLES: usize = 120;
const Y_SQUASH_BASE: f32 = 0.55; // monospace cell aspect for 2×2 supersample
const TILT: f32 = std::f32::consts::PI / 3.0;
const ROLL: f32 = std::f32::consts::PI / 4.0;

type M3 = [[f32; 3]; 3];
type V3 = [f32; 3];

fn rot_x(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}
fn rot_y(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}
fn rot_z(a: f32) -> M3 {
    let (c, s) = (a.cos(), a.sin());
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}
fn mul(a: &M3, b: &M3) -> M3 {
    let mut r = [[0.0_f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                r[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    r
}
fn apply(m: &M3, v: V3) -> V3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn base_orientations() -> (M3, M3) {
    (
        mul(&rot_z(ROLL), &rot_y(TILT)),
        mul(&rot_z(-ROLL), &rot_y(TILT)),
    )
}

fn ring_points(base: &M3, global: &M3, radius: f32, y_squash: f32) -> Vec<V3> {
    let m = mul(global, base);
    (0..RING_SAMPLES)
        .map(|i| {
            let t = (i as f32 / RING_SAMPLES as f32) * std::f32::consts::PI * 2.0;
            let p = apply(&m, [t.cos() * radius, t.sin() * radius, 0.0]);
            [p[0], p[1] * y_squash, p[2]]
        })
        .collect()
}

const BRAILLE_BITS: [[u32; 2]; 4] = [[1, 8], [2, 16], [4, 32], [64, 128]];

/// Render the BU logo as a vector of braille-encoded strings, one per cell row.
/// `rx`, `ry` are global rotations applied to the orbit-mark geometry.
pub fn render_braille_logo(
    w_cells: usize,
    h_cells: usize,
    radius: f32,
    stroke: f32,
    rx: f32,
    ry: f32,
) -> Vec<String> {
    let (base_a, base_b) = base_orientations();
    let sub_x = 2usize;
    let sub_y = 4usize;
    let sx = w_cells * sub_x;
    let sy = h_cells * sub_y;
    let cx = sx as f32 / 2.0;
    let cy = sy as f32 / 2.0;
    let y_squash = Y_SQUASH_BASE * (sub_y as f32 / 2.0);

    let global = mul(&rot_y(ry), &rot_x(rx));
    let pts_a = ring_points(&base_a, &global, radius, y_squash);
    let pts_b = ring_points(&base_b, &global, radius, y_squash);

    let stroke2 = stroke * stroke;
    let mut lines = Vec::with_capacity(h_cells);

    for cy_idx in 0..h_cells {
        let mut row = String::with_capacity(w_cells * 3);
        for cx_idx in 0..w_cells {
            let mut bits: u32 = 0;
            for dy in 0..sub_y {
                for dx in 0..sub_x {
                    let px = (cx_idx * sub_x + dx) as f32 - cx + 0.5;
                    let py = (cy_idx * sub_y + dy) as f32 - cy + 0.5;
                    let mut min2 = f32::INFINITY;
                    for p in &pts_a {
                        let dx = p[0] - px;
                        let dy = p[1] - py;
                        let d = dx * dx + dy * dy;
                        if d < min2 {
                            min2 = d;
                        }
                    }
                    for p in &pts_b {
                        let dx = p[0] - px;
                        let dy = p[1] - py;
                        let d = dx * dx + dy * dy;
                        if d < min2 {
                            min2 = d;
                        }
                    }
                    if min2 < stroke2 {
                        bits |= BRAILLE_BITS[dy][dx];
                    }
                }
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            row.push(ch);
        }
        lines.push(row);
    }
    lines
}

// ─────────────────────────── Layout ───────────────────────────

pub const LOGO_W: usize = 22;
pub const LOGO_H: usize = 9; // braille: 9 cells × 4 sub-rows = 36 sub-rows; ring max-y at R=14 is ~15.4, fits with margin
const LOGO_R: f32 = 14.0;
const LOGO_STROKE: f32 = 1.15;

// Boxed splash: logo, product name, version, shortcuts hint — all centered.
const LOGO_TO_TITLE_GAP: usize = 1;
const BANNER_TO_LOGO_GAP: usize = 1;
const VERSION_TO_HINT_GAP: usize = 2;
const TITLE: &str = "Browser Use";
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));
const HINT_PREFIX: &str = "press ";
const HINT_KEY: &str = "/";
const HINT_SUFFIX: &str = " for shortcuts";

fn hint_width() -> usize {
    HINT_PREFIX.chars().count() + HINT_KEY.chars().count() + HINT_SUFFIX.chars().count()
}

fn splash_block_h(banner_rows: usize) -> usize {
    // logo + gap + title(1) + version(1) + gap + hint(1)
    let banner_h = if banner_rows > 0 {
        banner_rows + BANNER_TO_LOGO_GAP
    } else {
        0
    };
    banner_h + LOGO_H + LOGO_TO_TITLE_GAP + 1 + 1 + VERSION_TO_HINT_GAP + 1
}

/// Compute the on-screen rect of the logo inside the welcome surface so the
/// mouse handler can hit-test clicks against just the logo, not the whole
/// panel.
pub fn logo_screen_rect(
    body_rect: ratatui::layout::Rect,
    has_status_notice: bool,
    banner_rows: usize,
) -> ratatui::layout::Rect {
    let status_notice_rows: u16 = if has_status_notice { 2 } else { 0 };
    const HEADER_H: u16 = 1;
    let target = body_rect.height.saturating_sub(status_notice_rows);
    let available_below_header = target.saturating_sub(HEADER_H);
    let block_h = splash_block_h(banner_rows) as u16;
    let pad_top = (available_below_header.saturating_sub(block_h) / 2).max(1);
    let banner_rows = if banner_rows > 0 {
        (banner_rows + BANNER_TO_LOGO_GAP) as u16
    } else {
        0
    };
    let top_offset = status_notice_rows + HEADER_H + pad_top + banner_rows;
    let col_offset = body_rect.width.saturating_sub(LOGO_W as u16) / 2;
    ratatui::layout::Rect {
        x: body_rect.x.saturating_add(col_offset),
        y: body_rect.y.saturating_add(top_offset),
        width: LOGO_W as u16,
        height: LOGO_H as u16,
    }
}

/// Build the welcome screen lines: header + centered boxed splash.
pub fn welcome_lines(
    width: u16,
    anim: &WelcomeAnim,
    _selected_idx: usize,
    target_h: u16,
    banner: Option<Vec<Line<'static>>>,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let width = width as usize;

    let block_h = splash_block_h(banner.as_ref().map_or(0, Vec::len));
    let header_h = 0_usize;
    let target = target_h as usize;
    let available_below_header = target.saturating_sub(header_h);
    let pad_top = (available_below_header.saturating_sub(block_h) / 2).max(1);

    for _ in 0..pad_top {
        out.push(Line::from(""));
    }

    if let Some(banner) = banner {
        for mut line in banner {
            center_line(&mut line, width);
            out.push(line);
        }
        for _ in 0..BANNER_TO_LOGO_GAP {
            out.push(Line::from(""));
        }
    }

    // Logo, centered.
    let logo_rows = render_braille_logo(LOGO_W, LOGO_H, LOGO_R, LOGO_STROKE, anim.rx, anim.ry);
    let logo_pad = " ".repeat(width.saturating_sub(LOGO_W) / 2);
    for row in logo_rows {
        let mut text = String::with_capacity(logo_pad.len() + row.len());
        text.push_str(&logo_pad);
        text.push_str(&row);
        out.push(Line::from(Span::styled(text, text_style())));
    }

    for _ in 0..LOGO_TO_TITLE_GAP {
        out.push(Line::from(""));
    }

    // Title — "Browser Use" bold, centered.
    let title_pad = " ".repeat(width.saturating_sub(TITLE.chars().count()) / 2);
    out.push(Line::from(vec![
        Span::raw(title_pad),
        Span::styled(TITLE.to_string(), bold()),
    ]));

    // Version — muted, centered.
    let version_pad = " ".repeat(width.saturating_sub(VERSION.chars().count()) / 2);
    out.push(Line::from(vec![
        Span::raw(version_pad),
        Span::styled(VERSION.to_string(), muted()),
    ]));

    for _ in 0..VERSION_TO_HINT_GAP {
        out.push(Line::from(""));
    }

    // "press / for shortcuts" — / is bold/accent, surrounding text muted.
    let hint_pad = " ".repeat(width.saturating_sub(hint_width()) / 2);
    out.push(Line::from(vec![
        Span::raw(hint_pad),
        Span::styled(HINT_PREFIX.to_string(), muted()),
        Span::styled(HINT_KEY.to_string(), bold()),
        Span::styled(HINT_SUFFIX.to_string(), muted()),
    ]));

    let used = header_h + pad_top + block_h;
    let pad_bottom = target.saturating_sub(used);
    for _ in 0..pad_bottom {
        out.push(Line::from(""));
    }

    out
}

fn center_line(line: &mut Line<'static>, width: usize) {
    let line_w = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let line_pad = " ".repeat(width.saturating_sub(line_w) / 2);
    line.spans.insert(0, Span::raw(line_pad));
}

// ─────────────────────── Session header ───────────────────────
const SESSION_TITLE: &str = "Browser Use Terminal";

/// Header block for an individual session: just the product title, left-
/// aligned. Rendered as plain committed scrollback text, so it sits at the top
/// of the conversation and scrolls away naturally as the session grows.
pub fn session_header_lines(_width: u16) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(SESSION_TITLE.to_string(), bold())),
        Line::from(""),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_header_is_left_aligned_title() {
        let lines = session_header_lines(80);
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Visualize during `cargo test -- --nocapture`.
        for line in &text {
            println!("|{}|", line.trim_end());
        }
        // Text-only header: title flush-left on the first row, no cwd.
        assert!(text[0].starts_with("Browser Use Terminal"));
    }
}
