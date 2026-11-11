//! Colours. Everything is a fixed 256-colour mid-tone, so themes can't remap it
//! and it holds at least 3:1 contrast on both white and black (see tests).
//! Text stays in the terminal's default foreground.
use ratatui::style::Color;
use std::sync::atomic::{AtomicBool, Ordering};

const fn c(index: u8) -> Color {
    Color::Indexed(index)
}

pub const GOOD: Color = c(29);
pub const WARN: Color = c(136);
pub const BAD: Color = c(160);
/// Empty tracks and unattributed memory.
pub const NEUTRAL: Color = c(244);

pub const APP: Color = c(33);
pub const WIRED: Color = c(166);
pub const COMPRESSED: Color = c(97);
pub const CACHED: Color = c(30);
/// All Claude Code sessions together.
pub const CLAUDE: Color = c(167);
/// The power history.
pub const POWER: Color = c(33);

/// Session identity, in slot order. Avoids the threshold hues.
pub const SESSIONS: [Color; 14] = [
    c(33),
    c(166),
    c(127),
    c(30),
    c(99),
    c(130),
    c(168),
    c(25),
    c(133),
    c(66),
    c(61),
    c(94),
    c(162),
    c(31),
];

/// Non-Claude consumers in the RAM strip.
pub const CONSUMERS: [Color; 3] = [c(101), c(138), c(65)];
/// The remaining non-Claude processes.
pub const OTHER: Color = c(67);

pub fn session(slot: usize) -> Color {
    SESSIONS[slot % SESSIONS.len()]
}

/// Load level: green, amber, red.
pub fn load(pct: f64) -> Color {
    match pct {
        ..60.0 => GOOD,
        ..85.0 => WARN,
        _ => BAD,
    }
}

const LEVELS: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];

/// Red, green and blue of a 256-colour index, 0-255. Other colours read as grey.
fn rgb(color: Color) -> [f64; 3] {
    match color {
        Color::Indexed(n @ 232..) => [8.0 + f64::from(n - 232) * 10.0; 3],
        Color::Indexed(n @ 16..) => {
            let n = usize::from(n - 16);
            [LEVELS[n / 36], LEVELS[n / 6 % 6], LEVELS[n % 6]]
        }
        _ => [128.0; 3],
    }
}

/// Whether the terminal takes 24-bit colour. Set once at startup.
static TRUECOLOR: AtomicBool = AtomicBool::new(false);

/// Turns smooth gradients on if `COLORTERM` promises 24-bit colour.
pub fn detect_truecolor() {
    let found = std::env::var("COLORTERM").is_ok_and(|v| v == "truecolor" || v == "24bit");
    TRUECOLOR.store(found, Ordering::Relaxed);
}

/// An in-between colour: exact with 24-bit colour, else the closest of the
/// 6x6x6 cube. Neither follows the theme.
fn blend(rgb: [f64; 3], truecolor: bool) -> Color {
    if truecolor {
        let [r, g, b] = rgb.map(|v| v.round().clamp(0.0, 255.0) as u8);
        return Color::Rgb(r, g, b);
    }
    let level = |v: f64| {
        (0..LEVELS.len())
            .min_by(|&a, &b| (LEVELS[a] - v).abs().total_cmp(&(LEVELS[b] - v).abs()))
            .unwrap_or(0) as u8
    };
    c(16 + 36 * level(rgb[0]) + 6 * level(rgb[1]) + level(rgb[2]))
}

fn mix(from: Color, to: Color, t: f64, truecolor: bool) -> Color {
    let (a, b) = (rgb(from), rgb(to));
    let t = t.clamp(0.0, 1.0);
    blend([0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t), truecolor)
}

fn shade_in(color: Color, factor: f64, truecolor: bool) -> Color {
    // The cube is too coarse to dim a colour without changing its hue.
    if !truecolor {
        return color;
    }
    blend(rgb(color).map(|v| v * factor.clamp(0.0, 1.0)), true)
}

fn heat_in(pct: f64, truecolor: bool) -> Color {
    const AMBER_AT: f64 = 60.0;
    if pct < AMBER_AT {
        mix(GOOD, WARN, pct / AMBER_AT, truecolor)
    } else {
        mix(WARN, BAD, (pct - AMBER_AT) / (100.0 - AMBER_AT), truecolor)
    }
}

/// `color` at `factor` of its brightness, for meters that fade in. Unchanged
/// without 24-bit colour.
pub fn shade(color: Color, factor: f64) -> Color {
    shade_in(color, factor, TRUECOLOR.load(Ordering::Relaxed))
}

/// Load as a ramp from green through amber to red, for gradients.
pub fn heat(pct: f64) -> Color {
    heat_in(pct, TRUECOLOR.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f64 {
        let Color::Indexed(16..) = color else {
            panic!("palette colours are indexed, and 0-15 follow the theme: {color:?}");
        };
        let [r, g, b] = rgb(color).map(|v| {
            let v = v / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn all() -> Vec<Color> {
        let mut colors = vec![
            GOOD, WARN, BAD, NEUTRAL, APP, WIRED, COMPRESSED, CACHED, CLAUDE,
        ];
        colors.extend(SESSIONS);
        colors.extend(CONSUMERS);
        colors.push(OTHER);
        colors
    }

    #[test]
    fn every_colour_reads_on_light_and_dark() {
        for color in all() {
            let l = luminance(color);
            let on_white = 1.05 / (l + 0.05);
            let on_black = (l + 0.05) / 0.05;
            assert!(on_white >= 3.0, "{color:?} on white: {on_white:.2}");
            assert!(on_black >= 3.0, "{color:?} on black: {on_black:.2}");
        }
    }

    #[test]
    fn thresholds() {
        assert_eq!(load(10.0), GOOD);
        assert_eq!(load(70.0), WARN);
        assert_eq!(load(99.0), BAD);
    }

    #[test]
    fn gradients_hit_their_ends_and_never_follow_the_theme() {
        for truecolor in [false, true] {
            assert_eq!(rgb_of(heat_in(0.0, truecolor)), rgb(GOOD));
            assert_eq!(rgb_of(heat_in(60.0, truecolor)), rgb(WARN));
            assert_eq!(rgb_of(heat_in(100.0, truecolor)), rgb(BAD));
            assert_eq!(rgb_of(shade_in(APP, 1.0, truecolor)), rgb(APP));
        }
        for pct in 0..=100 {
            assert!(matches!(
                heat_in(f64::from(pct), false),
                Color::Indexed(16..=231)
            ));
        }
        // Dimming needs 24-bit colour; the cube would shift the hue.
        assert_eq!(shade_in(APP, 0.5, false), APP);
        assert_eq!(shade_in(c(196), 0.5, true), Color::Rgb(128, 0, 0));
    }

    fn rgb_of(color: Color) -> [f64; 3] {
        match color {
            Color::Rgb(r, g, b) => [r, g, b].map(f64::from),
            other => rgb(other),
        }
    }

    #[test]
    fn slots_wrap() {
        assert_eq!(session(SESSIONS.len()), session(0));
    }
}
