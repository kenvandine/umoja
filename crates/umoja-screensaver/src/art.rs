//! What the screensaver draws: the word "umoja" made of dots, which fly in,
//! hold and fly out again with a different effect each cycle, inside slowly
//! turning rings of dots. The circles are a nod to Ubuntu's logo; the
//! effects to Omarchy's screensaver (TerminalTextEffects on its logo).
//!
//! Everything here is plain geometry over time so it can be unit-tested;
//! `main.rs` only paints the [`Frame`].

use std::f64::consts::{PI, TAU};

/// Effect names, as `--effect` takes them.
pub const EFFECTS: &[&str] = &["gather", "orbit", "rain", "spiral", "rings", "bloom"];

/// One cycle: fly in, hold, fly out.
pub const FORM: f64 = 3.5;
pub const HOLD: f64 = 7.0;
pub const LEAVE: f64 = 3.0;
pub const CYCLE: f64 = FORM + HOLD + LEAVE;

/// Ubuntu orange to aubergine.
const PALETTE: &[(f64, f64, f64)] = &[
    (0.914, 0.329, 0.125), // #E95420
    (0.871, 0.239, 0.255),
    (0.706, 0.157, 0.416),
    (0.467, 0.129, 0.435), // #77216F
];

/// A dot of the word: its place in the word (in cells, centred on 0,0) and
/// some per-dot randomness.
#[derive(Clone, Debug)]
pub struct Dot {
    pub tx: f64,
    pub ty: f64,
    pub seed: [f64; 4],
}

#[derive(Clone, Debug)]
pub struct Word {
    pub dots: Vec<Dot>,
    /// Size in cells.
    pub width: f64,
    pub height: f64,
}

/// Small deterministic PRNG (xorshift), so every monitor draws the same
/// show without sharing state.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Build a word from a coverage mask: `mask(col, row)` says whether that
/// cell is inside the letters. `cols` x `rows` cells.
pub fn word_from_mask(cols: usize, rows: usize, mask: impl Fn(usize, usize) -> bool) -> Word {
    let mut rng = Rng::new(7);
    let mut dots = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if mask(col, row) {
                dots.push(Dot {
                    tx: col as f64 - cols as f64 / 2.0 + 0.5,
                    ty: row as f64 - rows as f64 / 2.0 + 0.5,
                    seed: [rng.next(), rng.next(), rng.next(), rng.next()],
                });
            }
        }
    }
    Word {
        dots,
        width: cols as f64,
        height: rows as f64,
    }
}

/// One dot to paint, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mark {
    pub x: f64,
    pub y: f64,
    pub radius: f64,
    pub rgba: (f64, f64, f64, f64),
}

#[derive(Default)]
pub struct Frame {
    pub marks: Vec<Mark>,
}

/// Where the show is in its timeline.
#[derive(Clone, Copy, Debug)]
pub struct Cycle {
    pub effect: usize,
    /// Seconds into this cycle.
    pub t: f64,
    /// Where the word sits this cycle, as a fraction of the free space
    /// (0..1 each way). It moves every cycle so an OLED panel never shows
    /// the same pixels for long.
    pub place: (f64, f64),
}

/// The cycle at `elapsed` seconds. `forced` pins one effect.
pub fn cycle_at(elapsed: f64, forced: Option<usize>) -> Cycle {
    let index = (elapsed / CYCLE).floor().max(0.0) as u64;
    let n = EFFECTS.len();
    // Open with "gather", then step 1..n-1 effects on each cycle, so the
    // same effect never runs twice in a row.
    let mut effect = 0;
    for i in 1..=index {
        let step = 1 + (Rng::new(i + 1).next() * (n - 1) as f64) as usize % (n - 1);
        effect = (effect + step) % n;
    }
    if let Some(f) = forced {
        effect = f % n;
    }
    let mut rng = Rng::new(index + 1000);
    Cycle {
        effect,
        t: elapsed - index as f64 * CYCLE,
        // Wander, but stay near the middle: at the edges the composition
        // looked lopsided.
        place: (0.25 + 0.5 * rng.next(), 0.25 + 0.5 * rng.next()),
    }
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn ease_out_cubic(p: f64) -> f64 {
    1.0 - (1.0 - p).powi(3)
}

fn ease_in_out(p: f64) -> f64 {
    if p < 0.5 {
        4.0 * p * p * p
    } else {
        1.0 - (-2.0 * p + 2.0).powi(3) / 2.0
    }
}

fn ease_out_bounce(p: f64) -> f64 {
    let (n, d) = (7.5625, 2.75);
    if p < 1.0 / d {
        n * p * p
    } else if p < 2.0 / d {
        let p = p - 1.5 / d;
        n * p * p + 0.75
    } else if p < 2.5 / d {
        let p = p - 2.25 / d;
        n * p * p + 0.9375
    } else {
        let p = p - 2.625 / d;
        n * p * p + 0.984375
    }
}

fn lerp(a: f64, b: f64, p: f64) -> f64 {
    a + (b - a) * p
}

/// Colour along the palette, `u` wrapping 0..1.
pub fn palette(u: f64) -> (f64, f64, f64) {
    let u = u.rem_euclid(1.0) * 2.0; // there and back, so it never jumps
    let u = if u > 1.0 { 2.0 - u } else { u };
    let span = (PALETTE.len() - 1) as f64;
    let i = ((u * span).floor() as usize).min(PALETTE.len() - 2);
    let f = u * span - i as f64;
    let (a, b) = (PALETTE[i], PALETTE[i + 1]);
    (lerp(a.0, b.0, f), lerp(a.1, b.1, f), lerp(a.2, b.2, f))
}

pub struct Layout {
    pub width: f64,
    pub height: f64,
    /// Pixels per word cell.
    pub cell: f64,
    pub cx: f64,
    pub cy: f64,
}

/// Size the word to the screen and place it for this cycle.
pub fn layout(word: &Word, width: f64, height: f64, cycle: &Cycle) -> Layout {
    let cell = (width * 0.46 / word.width)
        .min(height * 0.26 / word.height)
        .max(1.0);
    let (ww, wh) = (word.width * cell, word.height * cell);
    // The word wanders over the screen (the rings may run off the edges).
    let margin = width.min(height) * 0.08;
    let free_x = (width - ww - 2.0 * margin).max(0.0);
    let free_y = (height - wh - 2.0 * margin).max(0.0);
    Layout {
        width,
        height,
        cell,
        cx: (width - free_x) / 2.0 + free_x * cycle.place.0,
        cy: (height - free_y) / 2.0 + free_y * cycle.place.1,
    }
}

/// Where a dot starts (and, with other seeds, leaves to) for an effect,
/// relative to its target, as a function of progress `p` (0 = away,
/// 1 = in place). Returns pixel coordinates.
fn path(effect: usize, d: &Dot, l: &Layout, tx: f64, ty: f64, p: f64, leaving: bool) -> (f64, f64) {
    let s = &d.seed;
    let (cx, cy) = (l.cx, l.cy);
    let far = l.width.max(l.height);
    match EFFECTS[effect] {
        "gather" => {
            let (sx, sy) = if leaving {
                (s[2] * l.width, s[3] * l.height)
            } else {
                (s[0] * l.width, s[1] * l.height)
            };
            let q = ease_out_cubic(p);
            (lerp(sx, tx, q), lerp(sy, ty, q))
        }
        "orbit" => {
            let (dx, dy) = (tx - cx, ty - cy);
            let rt = dx.hypot(dy);
            let at = dy.atan2(dx);
            let r0 = far * (0.35 + 0.25 * s[0]);
            let q = ease_in_out(p);
            let spin = if leaving { -1.0 } else { 1.0 } * (1.0 - q) * 2.5 * PI;
            let r = lerp(r0, rt, q);
            let a = at + spin;
            (cx + r * a.cos(), cy + r * a.sin())
        }
        "rain" => {
            if leaving {
                // Fall away off the bottom, faster for some dots.
                let q = (1.0 - p).powi(2);
                (tx, ty + q * (l.height - ty + 40.0) * (1.0 + s[1]))
            } else {
                let start = -40.0 - s[0] * l.height * 0.6;
                (tx, lerp(start, ty, ease_out_bounce(p)))
            }
        }
        "spiral" => {
            let (dx, dy) = (tx - cx, ty - cy);
            let rt = dx.hypot(dy);
            let at = dy.atan2(dx);
            let q = ease_out_cubic(p);
            let a = at - (1.0 - q) * 4.0 * PI;
            let r = rt * q;
            (cx + r * a.cos(), cy + r * a.sin())
        }
        "rings" => {
            // Sit on one of three turning rings, then step into the word.
            let ring = (s[0] * 3.0).floor();
            let radius = l.cell * (8.0 + 7.0 * ring) * 2.2;
            let a0 = s[1] * TAU;
            let turn = if ring as i64 % 2 == 0 { 1.0 } else { -1.0 };
            let q = ease_in_out(clamp01(p * 1.4 - 0.4));
            let a = a0 + turn * (1.0 - p) * 1.5 * PI;
            let (rx, ry) = (cx + radius * a.cos(), cy + radius * a.sin());
            (lerp(rx, tx, q), lerp(ry, ty, q))
        }
        _ => {
            // bloom: burst out from the centre to a wide circle, then in.
            let a = s[0] * TAU;
            let radius = far * (0.25 + 0.2 * s[1]);
            let (bx, by) = (cx + radius * a.cos(), cy + radius * a.sin());
            if p < 0.45 {
                let q = ease_out_cubic(p / 0.45);
                (lerp(cx, bx, q), lerp(cy, by, q))
            } else {
                let q = ease_in_out((p - 0.45) / 0.55);
                (lerp(bx, tx, q), lerp(by, ty, q))
            }
        }
    }
}

/// Everything to paint at `elapsed` seconds on a `width` x `height` screen.
pub fn frame(word: &Word, width: f64, height: f64, elapsed: f64, forced: Option<usize>) -> Frame {
    let cycle = cycle_at(elapsed, forced);
    let l = layout(word, width, height, &cycle);
    let mut marks = Vec::with_capacity(word.dots.len() + 160);

    // Fade the whole show in over the first second.
    let fade_in = clamp01(elapsed / 1.0);

    rings(&mut marks, word, &l, elapsed, fade_in);

    let t = cycle.t;
    let base_r = l.cell * 0.40;
    for d in &word.dots {
        let tx = l.cx + d.tx * l.cell;
        let ty = l.cy + d.ty * l.cell;
        // Stagger: dots on the left arrive (and leave) first.
        let stagger = 0.9 * ((d.tx / word.width) + 0.5) + 0.4 * d.seed[3];
        let (x, y, alpha, grow) = if t < FORM {
            let p = clamp01((t - stagger * 0.6) / (FORM - 1.0));
            let (x, y) = path(cycle.effect, d, &l, tx, ty, p, false);
            (x, y, 0.25 + 0.75 * p, 0.6 + 0.4 * p)
        } else if t < FORM + HOLD {
            // Breathe: a wave of size and brightness runs along the word.
            let h = t - FORM;
            let wave = (h * 2.2 - d.tx * 0.22).sin();
            (tx, ty, 0.85 + 0.15 * wave, 1.0 + 0.16 * wave)
        } else {
            let lt = t - FORM - HOLD;
            let p = 1.0 - clamp01((lt - stagger * 0.5) / (LEAVE - 0.9));
            let (x, y) = path(cycle.effect, d, &l, tx, ty, p, true);
            (x, y, p, 0.5 + 0.5 * p)
        };
        let u = (d.tx / word.width + 0.5) * 0.7 + elapsed * 0.035;
        let (r, g, b) = palette(u);
        marks.push(Mark {
            x,
            y,
            radius: base_r * grow,
            rgba: (r, g, b, alpha * fade_in),
        });
    }
    Frame { marks }
}

/// Three rings of small dots turning slowly around the word.
fn rings(marks: &mut Vec<Mark>, word: &Word, l: &Layout, elapsed: f64, fade_in: f64) {
    let base = word.width * l.cell * 0.5;
    for (k, (scale, count, speed)) in [(1.18, 56, 0.05), (1.36, 72, -0.035), (1.56, 90, 0.025)]
        .into_iter()
        .enumerate()
    {
        let radius = base * scale;
        for i in 0..count {
            let a = i as f64 / count as f64 * TAU + elapsed * speed * TAU;
            // A gap in each ring sweeps round, so the rings read as moving.
            let gap = ((a - elapsed * 0.3 - k as f64 * 2.1).rem_euclid(TAU) / TAU).powf(1.6);
            let (r, g, b) = palette(0.15 + 0.25 * k as f64 + elapsed * 0.02);
            marks.push(Mark {
                x: l.cx + radius * a.cos(),
                y: l.cy + radius * a.sin(),
                radius: (l.cell * 0.22).max(1.2) * (0.6 + 0.7 * gap),
                rgba: (r, g, b, (0.10 + 0.50 * gap) * fade_in),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> Word {
        // A 20x5 block with a hole: enough to exercise every path.
        word_from_mask(20, 5, |c, r| !(c == 10 && r == 2))
    }

    #[test]
    fn mask_becomes_centred_dots() {
        let w = bar();
        assert_eq!(w.dots.len(), 99);
        let mean: f64 = w.dots.iter().map(|d| d.tx).sum::<f64>() / w.dots.len() as f64;
        assert!(mean.abs() < 0.2);
    }

    #[test]
    fn every_effect_lands_on_the_word() {
        let w = bar();
        for (e, name) in EFFECTS.iter().enumerate() {
            let f = frame(&w, 1920.0, 1080.0, FORM + 0.5, Some(e));
            let c = cycle_at(FORM + 0.5, Some(e));
            let l = layout(&w, 1920.0, 1080.0, &c);
            let dots = &f.marks[f.marks.len() - w.dots.len()..];
            for (m, d) in dots.iter().zip(&w.dots) {
                assert!((m.x - (l.cx + d.tx * l.cell)).abs() < 1e-6, "{name}");
                assert!((m.y - (l.cy + d.ty * l.cell)).abs() < 1e-6, "{name}");
            }
        }
    }

    #[test]
    fn word_stays_on_screen_while_held() {
        let w = bar();
        for i in 0..40 {
            let at = i as f64 * CYCLE + FORM + 1.0;
            let f = frame(&w, 2880.0, 1800.0, at, None);
            for m in &f.marks[f.marks.len() - w.dots.len()..] {
                assert!(m.x > 0.0 && m.x < 2880.0 && m.y > 0.0 && m.y < 1800.0);
            }
        }
    }

    #[test]
    fn effects_change_every_cycle_and_place_drifts() {
        let mut last = None;
        let mut places = Vec::new();
        for i in 0..12 {
            let c = cycle_at(i as f64 * CYCLE + 1.0, None);
            assert_ne!(Some(c.effect), last);
            last = Some(c.effect);
            places.push(c.place);
        }
        assert_eq!(cycle_at(0.5, None).effect, 0);
        assert!(places.windows(2).all(|p| p[0] != p[1]));
    }

    #[test]
    fn palette_is_continuous() {
        let mut prev = palette(0.0);
        for i in 1..=200 {
            let c = palette(i as f64 / 100.0);
            assert!((c.0 - prev.0).abs() < 0.05 && (c.2 - prev.2).abs() < 0.05);
            prev = c;
        }
    }
}
