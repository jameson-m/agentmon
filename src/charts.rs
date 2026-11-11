//! Cell-level graphics: braille history graphs, and proportional bars smoothed
//! with eighth-block glyphs.
use crate::palette;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

const UP: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const RIGHT: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
const TRACK: &str = "░";
const CORE_IDLE: f64 = 5.0;

/// Braille dot bits, bottom to top, for the left and right half of a cell.
const DOTS: [[u32; 4]; 2] = [[0x40, 0x04, 0x02, 0x01], [0x80, 0x20, 0x10, 0x08]];

/// A cell with `left` and `right` dots, 0-4 each, filled from the bottom, or
/// from the top if `down`.
pub fn braille(left: usize, right: usize, down: bool) -> char {
    let bits = |side: usize, n: usize| -> u32 {
        let column = DOTS[side];
        if down {
            column.iter().rev().take(n).sum()
        } else {
            column.iter().take(n).sum()
        }
    };
    char::from_u32(0x2800 + bits(0, left) + bits(1, right)).unwrap_or(' ')
}

/// Filled braille history, newest at the right, two samples per cell. Every
/// sample shows at least one dot. It grows up from the bottom edge, or with
/// `down`, hangs from the top, to mirror another graph. With `by_height`, cells
/// are coloured by how far they are from the baseline, capped at the column's
/// value, so peaks stand out; otherwise by the newer sample's own colour.
pub fn graph(
    buf: &mut Buffer,
    rect: Rect,
    samples: &[(f64, Color)],
    max: f64,
    by_height: Option<fn(f64) -> Color>,
    down: bool,
) {
    if rect.is_empty() || max <= 0.0 {
        return;
    }
    let slots = rect.width as usize * 2;
    let shown = &samples[samples.len().saturating_sub(slots)..];
    let blank = slots - shown.len();
    let rows = rect.height as usize;
    let dots = rows * 4;
    let level = |value: f64| ((value / max * dots as f64).round() as usize).clamp(1, dots);
    for (col, x) in (rect.left()..rect.right()).enumerate() {
        let pick = |slot: usize| slot.checked_sub(blank).and_then(|i| shown.get(i));
        let (left, right) = (pick(col * 2), pick(col * 2 + 1));
        let Some(newest) = right.or(left) else {
            continue;
        };
        let peak = left.map_or(0.0, |s| s.0).max(right.map_or(0.0, |s| s.0));
        let (left, right) = (
            left.map_or(0, |s| level(s.0)),
            right.map_or(0, |s| level(s.0)),
        );
        for row in 0..rows {
            let (left, right) = (left.saturating_sub(row * 4), right.saturating_sub(row * 4));
            if left + right == 0 {
                break;
            }
            let color = match by_height {
                Some(tint) => tint(peak.min((row + 1) as f64 / rows as f64 * max)),
                None => newest.1,
            };
            let y = if down {
                rect.top() + row as u16
            } else {
                rect.bottom() - 1 - row as u16
            };
            buf[(x, y)]
                .set_char(braille(left, right, down))
                .set_fg(color);
        }
    }
}

/// One row-high history, for inline use.
pub fn spark(buf: &mut Buffer, rect: Rect, samples: &[(f64, Color)], max: f64) {
    let clipped: Vec<(f64, Color)> = samples.iter().map(|&(v, c)| (v.min(max), c)).collect();
    graph(buf, Rect { height: 1, ..rect }, &clipped, max, None, false);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub value: f64,
    /// None is empty track.
    pub color: Option<Color>,
}

pub fn segment(value: f64, color: Color) -> Segment {
    Segment {
        value,
        color: Some(color),
    }
}

/// What one bar cell shows: the left colour up to `eighths`, then the right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub left: Option<Color>,
    pub right: Option<Color>,
    pub eighths: usize,
}

/// Lays segments out over `width` cells, scaled to `total`. A cell holds two
/// colours at most, so a segment narrower than a cell can vanish.
pub fn bar_cells(segments: &[Segment], total: f64, width: usize) -> Vec<Cell> {
    let mut stops = Vec::with_capacity(segments.len());
    let mut end = 0.0;
    if total > 0.0 {
        for s in segments {
            end += s.value / total * width as f64;
            stops.push((end, s.color));
        }
    }
    let at = |position: f64| {
        stops
            .iter()
            .find(|(end, _)| *end > position)
            .and_then(|(_, color)| *color)
    };
    (0..width)
        .map(|x| {
            let start = x as f64;
            let left = at(start + 1e-9);
            let right = at(start + 1.0 - 1e-9);
            let split = stops
                .iter()
                .find(|(end, _)| *end > start + 1e-9)
                .map_or(start, |(end, _)| *end);
            let eighths = (((split - start) * 8.0).round() as usize).min(8);
            if left == right {
                Cell {
                    left,
                    right,
                    eighths: 8,
                }
            } else {
                Cell {
                    left,
                    right,
                    eighths,
                }
            }
        })
        .collect()
}

/// Draws a bar. Uncoloured cells show the track if `track`, else stay blank.
pub fn bar(buf: &mut Buffer, rect: Rect, segments: &[Segment], total: f64, track: bool) {
    let cells = bar_cells(segments, total, rect.width as usize);
    for y in rect.top()..rect.bottom() {
        for (cell, x) in cells.iter().zip(rect.left()..) {
            let out = &mut buf[(x, y)];
            // A sliver too thin to draw gives the cell to what follows it.
            let left = cell.left.filter(|_| cell.eighths > 0);
            match (left, cell.right) {
                (None, None) => {
                    if track {
                        out.set_symbol(TRACK).set_fg(palette::NEUTRAL);
                    }
                }
                (Some(left), right) => {
                    out.set_symbol(RIGHT[cell.eighths]).set_fg(left);
                    if let Some(right) = right {
                        out.set_bg(right);
                    }
                }
                (None, Some(right)) if cell.left.is_some() => {
                    out.set_symbol(RIGHT[8]).set_fg(right);
                }
                (None, Some(right)) => {
                    out.set_symbol(RIGHT[8 - cell.eighths.min(8)]).set_fg(right);
                }
            }
        }
    }
}

/// How a meter's filled cells are coloured along its length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ramp {
    /// Each segment's colour, dark to light across the filled part.
    Fade,
    /// Green through amber to red, whatever the segment colours.
    Heat,
}

/// A half block, so neighbouring cells join into one solid bar.
const METER_CELL: &str = "▄";
/// Brightness at a fading meter's left end.
const FADE_FROM: f64 = 0.6;

/// A meter of square cells: filled ones coloured by `ramp`, the rest dim.
pub fn meter(buf: &mut Buffer, rect: Rect, segments: &[Segment], total: f64, ramp: Ramp) {
    let width = rect.width as usize;
    let cells: Vec<Option<Color>> = (0..width)
        .map(|i| {
            let middle = (i as f64 + 0.5) / width as f64 * total;
            let mut end = 0.0;
            segments
                .iter()
                .find_map(|s| {
                    end += s.value;
                    (end > middle).then_some(s.color)
                })
                .flatten()
        })
        .collect();
    let filled = cells.iter().flatten().count();
    // How far along `of` cells cell `i` is.
    let along = |i: usize, of: usize| {
        if of > 1 {
            (i as f64 / (of - 1) as f64).min(1.0)
        } else {
            1.0
        }
    };
    for ((i, cell), x) in cells.iter().enumerate().zip(rect.left()..rect.right()) {
        let style = match (cell, ramp) {
            (None, _) => style(palette::NEUTRAL).add_modifier(Modifier::DIM),
            // Heat means the level, so it follows the track.
            (Some(_), Ramp::Heat) => style(palette::heat(along(i, width) * 100.0)),
            // A fade is decoration, so the filled part shows all of it.
            (Some(color), Ramp::Fade) => {
                let factor = FADE_FROM + (1.0 - FADE_FROM) * along(i, filled);
                style(palette::shade(*color, factor))
            }
        };
        for y in rect.top()..rect.bottom() {
            buf[(x, y)].set_symbol(METER_CELL).set_style(style);
        }
    }
}

/// One bar per core, its height and colour the load. Idle cores are grey, so
/// the eye goes to the busy ones. Cores share a bar when there isn't room.
pub fn cores(buf: &mut Buffer, rect: Rect, cores: &[f64]) {
    if cores.is_empty() || rect.is_empty() {
        return;
    }
    let per = cores.len().div_ceil(rect.width as usize);
    for (group, x) in cores.chunks(per).zip(rect.left()..) {
        let pct = group.iter().sum::<f64>() / group.len() as f64;
        let level = ((pct / 100.0 * 8.0).round() as usize).clamp(1, 8);
        let color = if pct < CORE_IDLE {
            palette::NEUTRAL
        } else {
            palette::load(pct)
        };
        buf[(x, rect.y)].set_symbol(UP[level]).set_fg(color);
    }
}

/// Coloured swatch for legends.
pub const SWATCH: &str = "■";

pub fn style(color: Color) -> Style {
    Style::new().fg(color)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Color = Color::Indexed(33);
    const B: Color = Color::Indexed(166);

    fn split(text: &Buffer) -> Vec<String> {
        let area = text.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| text[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn braille_fills_from_the_bottom() {
        assert_eq!(braille(0, 0, false), '\u{2800}');
        assert_eq!(braille(1, 0, false), '⡀');
        assert_eq!(braille(1, 0, true), '⠁');
        assert_eq!(braille(4, 2, true), '⡟');
        assert_eq!(braille(4, 2, false), '⣧');
        assert_eq!(braille(9, 9, false), '⣿');
    }

    #[test]
    fn graph_is_right_aligned_two_samples_a_cell() {
        let rect = Rect::new(0, 0, 3, 2);
        let mut buf = Buffer::empty(rect);
        graph(
            &mut buf,
            rect,
            &[(100.0, A), (50.0, B), (0.0, B)],
            100.0,
            None,
            false,
        );
        assert_eq!(split(&buf), [" ⢸ ", " ⢸⣇"]);
        assert_eq!(buf[(1, 1)].fg, A);
        assert_eq!(buf[(2, 1)].fg, B);
    }

    #[test]
    fn graph_tints_by_height_up_to_the_value() {
        let rect = Rect::new(0, 0, 1, 4);
        let mut buf = Buffer::empty(rect);
        graph(
            &mut buf,
            rect,
            &[(100.0, A)],
            100.0,
            Some(palette::load),
            false,
        );
        assert_eq!(buf[(0, 3)].fg, palette::GOOD);
        assert_eq!(buf[(0, 1)].fg, palette::WARN);
        assert_eq!(buf[(0, 0)].fg, palette::BAD);
        // One row still goes red.
        let rect = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(rect);
        graph(
            &mut buf,
            rect,
            &[(500.0, A)],
            100.0,
            Some(palette::load),
            false,
        );
        assert_eq!(buf[(0, 0)].fg, palette::BAD);
    }

    #[test]
    fn graph_can_hang_from_the_top() {
        let rect = Rect::new(0, 0, 1, 2);
        let mut buf = Buffer::empty(rect);
        graph(
            &mut buf,
            rect,
            &[(25.0, A), (75.0, A)],
            100.0,
            Some(palette::load),
            true,
        );
        assert_eq!(split(&buf), ["⢻", "⠘"]);
        assert_eq!(buf[(0, 0)].fg, palette::GOOD);
        assert_eq!(buf[(0, 1)].fg, palette::WARN);
    }

    #[test]
    fn spark_clips_to_its_row() {
        let rect = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(rect);
        spark(&mut buf, rect, &[(500.0, A)], 100.0);
        assert_eq!(split(&buf), ["⢸"]);
    }

    #[test]
    fn bar_splits_a_cell_between_two_colours() {
        let cells = bar_cells(&[segment(1.5, A), segment(2.5, B)], 4.0, 4);
        assert_eq!(cells[0].left, Some(A));
        assert_eq!(cells[0].eighths, 8);
        assert_eq!(
            cells[1],
            Cell {
                left: Some(A),
                right: Some(B),
                eighths: 4
            }
        );
        assert_eq!(cells[3].left, Some(B));
    }

    #[test]
    fn bar_past_the_segments_is_track() {
        let cells = bar_cells(&[segment(1.0, A)], 4.0, 4);
        assert_eq!(cells[0].left, Some(A));
        assert_eq!(cells[3].left, None);
        assert_eq!(bar_cells(&[], 0.0, 2)[0].left, None);
    }

    #[test]
    fn meter_fills_by_cell_and_fades_in() {
        let rect = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(rect);
        meter(
            &mut buf,
            rect,
            &[segment(1.0, A), segment(1.0, B)],
            4.0,
            Ramp::Fade,
        );
        assert_eq!(split(&buf), ["▄▄▄▄"]);
        assert_eq!(buf[(2, 0)].fg, palette::NEUTRAL);
        meter(&mut buf, rect, &[segment(4.0, A)], 4.0, Ramp::Fade);
        assert_eq!(buf[(3, 0)].fg, A);
        meter(&mut buf, rect, &[segment(4.0, A)], 4.0, Ramp::Heat);
        assert_eq!(buf[(0, 0)].fg, palette::GOOD);
        assert_eq!(buf[(3, 0)].fg, palette::BAD);
    }

    #[test]
    fn cores_show_load_as_height_and_share_when_narrow() {
        let rect = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(rect);
        cores(&mut buf, rect, &[100.0, 50.0, 0.0]);
        assert_eq!(split(&buf), ["█▄▁ "]);
        assert_eq!(buf[(0, 0)].fg, palette::BAD);
        assert_eq!(buf[(2, 0)].fg, palette::NEUTRAL);
        let rect = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(rect);
        cores(&mut buf, rect, &[100.0, 100.0, 0.0, 0.0]);
        assert_eq!(split(&buf), ["█▁"]);
    }
}
