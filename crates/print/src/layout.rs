//! The formatted body on paper: [`mailcraft_mime::RichText`] lines (headings, bold, italic,
//! colours, links, highlights) and [`mailcraft_mime::layout`] tables (columns sized as the
//! reading pane sizes them, cell and table backgrounds, boxes placed over others), laid out into
//! one tall column of [`Item`]s that the writer cuts into pages. Lines never cross a page break
//! and a table row that fits a page is kept on one; cell backgrounds are cut with the page.
//! Everything here comes from mail, so depths, counts and sizes are capped.

use mailcraft_mime::Span;
use mailcraft_mime::layout::{Align, Block, Cell, Overlay, Table, Width};

use crate::Faces;

/// A CSS pixel on paper, in points (96 pixels to the inch, 72 points).
pub(crate) const PX: f32 = 0.75;
/// The space inside a table cell, each side.
const CELL_PAD: f32 = 4.5;
/// Narrower than this, a column whose content doesn't fit goes under the others.
const MIN_COLUMN: f32 = 105.0;
/// Tables nested deeper than this are left out.
const MAX_DEPTH: usize = 16;
/// Items laid out at most (a line of text, a filled box…).
const MAX_ITEMS: usize = 400_000;
/// Body text size and the gap an empty line leaves.
pub(crate) const BODY: f32 = 11.0;
const PARAGRAPH_GAP: f32 = 6.0;
const INK: [u8; 3] = [0, 0, 0];
const LINK: [u8; 3] = [0x0b, 0x5c, 0xb3];
const FAINT: [u8; 3] = [0x70, 0x70, 0x70];

/// A line of text in one style: `top` is where its line box starts (the baseline is `size`
/// below), in the tall column's coordinates (down from 0).
pub(crate) struct Text {
    pub x: f32,
    pub top: f32,
    pub style: usize,
    pub size: f32,
    pub italic: bool,
    pub underline: bool,
    pub color: [u8; 3],
    pub link: Option<String>,
    pub text: String,
    pub width: f32,
}

/// A filled box.
pub(crate) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [u8; 3],
}

pub(crate) enum Item {
    Text(Text),
    Rect(Rect),
    /// A thin grey line across.
    Rule {
        x: f32,
        y: f32,
        w: f32,
    },
}

/// One tall column, `page_h` to a page, that the body is laid out into.
pub(crate) struct Canvas<'f, 'a> {
    faces: &'f Faces<'a>,
    page_h: f32,
    pub items: Vec<Item>,
    /// Above 0 while only measuring: nothing is kept and nothing moves to the next page.
    measuring: usize,
}

/// A word of a line as it will be drawn.
struct Frag<'s> {
    span: &'s Span,
    text: String,
    width: f32,
    size: f32,
    style: usize,
    /// How the text looks: a picture's description, drawn faint.
    picture: bool,
}

/// The size, in points, text in a span is drawn at.
fn span_size(sp: &Span) -> f32 {
    match sp.heading {
        0 if sp.code => 10.0,
        0 => BODY,
        1 => 20.0,
        2 => 16.0,
        3 => 14.0,
        _ => 12.0,
    }
}

fn span_style(sp: &Span) -> usize {
    usize::from(sp.bold || sp.heading > 0)
}

/// The colour text is drawn in: its own (a link's blue), made readable on `page` when it would
/// vanish into it (white text whose dark band isn't drawn, a light grey on white).
fn ink(c: Option<[u8; 3]>, link: bool, page: [u8; 3]) -> [u8; 3] {
    let lum = |c: [u8; 3]| (0.2126 * f32::from(c[0]) + 0.7152 * f32::from(c[1]) + 0.0722 * f32::from(c[2])) / 255.0;
    let c = c.unwrap_or(if link { LINK } else { INK });
    if (lum(c) - lum(page)).abs() < 0.3 { if lum(page) > 0.5 { INK } else { [255, 255, 255] } } else { c }
}

impl<'f, 'a> Canvas<'f, 'a> {
    pub fn new(faces: &'f Faces<'a>, page_h: f32) -> Canvas<'f, 'a> {
        Canvas { faces, page_h: page_h.max(1.0), items: Vec::new(), measuring: 0 }
    }

    fn push(&mut self, item: Item) {
        if self.measuring == 0 && self.items.len() < MAX_ITEMS {
            self.items.push(item);
        }
    }

    /// A box behind content laid out from item `slot` on (so it is drawn first).
    fn behind(&mut self, slot: usize, rect: Rect) {
        if self.measuring == 0 && self.items.len() < MAX_ITEMS && rect.h > 0.0 && rect.w > 0.0 {
            self.items.insert(slot.min(self.items.len()), Item::Rect(rect));
        }
    }

    /// Where something `h` tall starting at `y` goes so that it stays on one page: `y`, or the
    /// top of the next page when it would cross the break (something taller than a page stays).
    pub fn fit(&self, y: f32, h: f32) -> f32 {
        if self.measuring > 0 || h >= self.page_h || !y.is_finite() {
            return y;
        }
        let top = (y / self.page_h).floor() * self.page_h;
        if y + h > top + self.page_h + 0.01 { top + self.page_h } else { y }
    }

    pub fn text_width(&self, style: usize, s: &str, size: f32) -> f32 {
        self.faces.text_width(style, s, size)
    }

    /// One line of plain text; its bottom.
    #[allow(clippy::too_many_arguments)]
    pub fn line(&mut self, x: f32, y: f32, style: usize, size: f32, lead: f32, color: [u8; 3], text: &str) -> f32 {
        let y = self.fit(y, lead);
        let width = self.text_width(style, text, size);
        self.push(Item::Text(Text { x, top: y, style, size, italic: false, underline: false, color, link: None, text: text.to_string(), width }));
        y + lead
    }

    pub fn rule(&mut self, x: f32, y: f32, w: f32) {
        self.push(Item::Rule { x, y, w });
    }

    /// The words of a formatted line, each with its width (a word wider than `width` is cut).
    fn fragments<'s>(&self, line: &'s [Span], width: f32) -> Vec<Frag<'s>> {
        let mut out: Vec<Frag<'s>> = Vec::new();
        for sp in line {
            let (size, style) = (span_size(sp), span_style(sp));
            let (text, picture) = match &sp.image {
                Some(_) if sp.text.trim().is_empty() => continue,
                Some(_) => (format!("[{}]", sp.text.trim()), true),
                None => (sp.text.clone(), false),
            };
            let text = crate::clean(&text).replace(['\n', '\t'], " ");
            for word in text.split_inclusive(' ') {
                if out.len() > 20_000 {
                    return out;
                }
                let w = self.text_width(style, word, size);
                // A picture's description stands in for it: left out where it wouldn't fit.
                if picture && w > width {
                    continue;
                }
                if w <= width || word.chars().count() < 2 {
                    out.push(Frag { span: sp, text: word.to_string(), width: w, size, style, picture });
                    continue;
                }
                let (mut piece, mut piece_w) = (String::new(), 0.0f32);
                for c in word.chars() {
                    let cw = self.faces.char_width(style, c, size);
                    if piece_w + cw > width && !piece.is_empty() {
                        out.push(Frag { span: sp, text: std::mem::take(&mut piece), width: piece_w, size, style, picture });
                        piece_w = 0.0;
                    }
                    piece.push(c);
                    piece_w += cw;
                }
                if !piece.is_empty() {
                    out.push(Frag { span: sp, text: piece, width: piece_w, size, style, picture });
                }
            }
        }
        out
    }

    /// How wide a formatted line is on one line (a picture, drawn as its description or not at
    /// all, asks for nothing).
    fn line_width(&self, line: &[Span]) -> f32 {
        line.iter()
            .map(|sp| match &sp.image {
                Some(_) => 0.0,
                None => self.text_width(span_style(sp), &crate::clean(&sp.text), span_size(sp)),
            })
            .sum()
    }

    /// Formatted lines `width` wide at `x` from `y`, lined up by `align`, on a background of
    /// `page`; the bottom.
    pub fn lines(&mut self, lines: &[Vec<Span>], x: f32, mut y: f32, width: f32, page: [u8; 3], align: Align) -> f32 {
        let width = width.max(1.0);
        for line in lines {
            if line.is_empty() {
                y += PARAGRAPH_GAP;
                continue;
            }
            let frags = self.fragments(line, width);
            if frags.is_empty() {
                continue;
            }
            // Greedy fill into visual lines (a trailing space doesn't count against the width).
            let mut rows: Vec<Vec<Frag<'_>>> = vec![Vec::new()];
            let mut used = 0.0f32;
            for f in frags {
                let trailing = if f.text.ends_with(' ') { self.text_width(f.style, " ", f.size) } else { 0.0 };
                let fits = used + f.width - trailing <= width + 0.01;
                if !fits && rows.last().is_some_and(|r| !r.is_empty()) {
                    rows.push(Vec::new());
                    used = 0.0;
                }
                used += f.width;
                if let Some(r) = rows.last_mut() {
                    r.push(f);
                }
            }
            for row in rows.iter().filter(|r| !r.is_empty()) {
                let size = row.iter().map(|f| f.size).fold(0.0f32, f32::max);
                let lead = size * 1.32;
                y = self.fit(y, lead);
                let trailing = row.last().filter(|f| f.text.ends_with(' ')).map_or(0.0, |f| self.text_width(f.style, " ", f.size));
                let row_w: f32 = row.iter().map(|f| f.width).sum::<f32>() - trailing;
                let mut cx = x + match align {
                    Align::Left => 0.0,
                    Align::Center => ((width - row_w) / 2.0).max(0.0),
                    Align::Right => (width - row_w).max(0.0),
                };
                // Words of one span together, so the text copies whole.
                let mut runs: Vec<(&Frag<'_>, String, f32)> = Vec::new();
                for f in row {
                    match runs.last_mut() {
                        Some((first, text, w)) if std::ptr::eq(first.span, f.span) && first.picture == f.picture => {
                            text.push_str(&f.text);
                            *w += f.width;
                        }
                        _ => runs.push((f, f.text.clone(), f.width)),
                    }
                }
                for (f, text, width) in runs {
                    let link = f.span.link.as_deref().filter(|l| {
                        let l = l.to_ascii_lowercase();
                        l.len() < 2048 && (l.starts_with("https://") || l.starts_with("http://") || l.starts_with("mailto:"))
                    });
                    let color = if f.picture { FAINT } else { ink(f.span.color, f.span.link.is_some(), page) };
                    if let Some(bg) = f.span.background {
                        self.push(Item::Rect(Rect { x: cx, y, w: width, h: lead, color: bg }));
                    }
                    self.push(Item::Text(Text {
                        x: cx,
                        top: y,
                        style: f.style,
                        size: f.size,
                        italic: f.span.italic,
                        underline: f.span.link.is_some(),
                        color,
                        link: link.map(str::to_string),
                        text,
                        width,
                    }));
                    cx += width;
                }
                y += lead;
            }
        }
        y
    }

    /// Laid-out parts `width` wide at `x` from `y`; the bottom.
    #[allow(clippy::too_many_arguments)]
    pub fn blocks(&mut self, blocks: &[Block], x: f32, mut y: f32, width: f32, page: [u8; 3], align: Align, depth: usize) -> f32 {
        for b in blocks {
            match b {
                Block::Text(lines) => y = self.lines(lines, x, y, width, page, align),
                Block::Table(t) if depth < MAX_DEPTH => y = self.table(t, x, y, width, page, depth),
                Block::Table(_) => {}
            }
        }
        y
    }

    /// How wide laid-out parts are unwrapped (the widest line, or the widest table): CSS's
    /// max-content width. `None` past the depth tables are drawn to.
    fn natural(&self, blocks: &[Block], depth: usize) -> Option<f32> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut widest = 0.0f32;
        for b in blocks {
            match b {
                Block::Text(lines) => {
                    for line in lines {
                        widest = widest.max(self.line_width(line));
                    }
                }
                Block::Table(t) => {
                    let w = match t.width {
                        Some(Width::Px(px)) => px as f32 * PX,
                        _ => {
                            let measured: Vec<Vec<Option<f32>>> =
                                t.rows.iter().map(|r| r.iter().map(|c| self.natural(&c.blocks, depth + 1)).collect()).collect();
                            table_natural(&t.rows, &column_widths(&t.rows), &column_naturals(&t.rows, &measured), &measured)?
                        }
                    };
                    widest = widest.max(t.max_width.map_or(w, |m| w.min(m as f32 * PX)));
                }
            }
        }
        Some(widest)
    }

    /// A table at `x` from `y` in `width`; its bottom.
    fn table(&mut self, t: &Table, x: f32, y: f32, width: f32, page: [u8; 3], depth: usize) -> f32 {
        let columns = column_widths(&t.rows);
        let measured: Vec<Vec<Option<f32>>> = t.rows.iter().map(|r| r.iter().map(|c| self.natural(&c.blocks, depth + 1)).collect()).collect();
        let natural = column_naturals(&t.rows, &measured);
        let tw = match t.width {
            Some(Width::Px(px)) => (px as f32 * PX).min(width),
            Some(Width::Percent(pc)) => width * f32::from(pc) / 100.0,
            None => table_natural(&t.rows, &columns, &natural, &measured).map_or(width, |n| n.min(width)),
        };
        let tw = t.max_width.map_or(tw, |m| tw.min(m as f32 * PX)).max(1.0);
        let x0 = x + match t.align {
            Align::Center => (width - tw) / 2.0,
            Align::Right => width - tw,
            Align::Left => 0.0,
        }
        .max(0.0);
        let page = t.background.unwrap_or(page);
        // At least a line of the first row goes with the table's top.
        let top = self.fit(y, BODY * 1.32 + 2.0 * CELL_PAD);
        let slot = self.items.len();
        let mut y = top;
        for (row, m) in t.rows.iter().zip(&measured) {
            y = self.row(row, &columns, &natural, m, x0, y, tw, page, depth);
        }
        if let Some(bg) = t.background {
            self.behind(slot, Rect { x: x0, y: top, w: tw, h: y - top, color: bg });
        }
        y
    }

    /// A table row: its cells side by side (one under the other when there's no room), kept on
    /// one page when it fits one; its bottom.
    #[allow(clippy::too_many_arguments)]
    fn row(
        &mut self,
        row: &[Cell],
        columns: &[Option<Width>],
        natural: &[Option<f32>],
        measured: &[Option<f32>],
        x: f32,
        y: f32,
        width: f32,
        page: [u8; 3],
        depth: usize,
    ) -> f32 {
        let widths = row_widths(row, columns, natural, measured, width, |w| match w {
            Width::Px(px) => px as f32 * PX,
            Width::Percent(pc) => width * f32::from(pc) / 100.0,
        });
        let stack = row.len() > 1
            && widths.iter().zip(row).enumerate().any(|(i, (w, c))| {
                *w < MIN_COLUMN && !c.blocks.is_empty() && measured.get(i).copied().flatten().is_none_or(|n| *w < n + 2.0 * CELL_PAD)
            });
        let mut y = y;
        if self.measuring == 0 {
            self.measuring += 1;
            let h = self.cells(row, &widths, stack, x, y, width, page, depth) - y;
            self.measuring -= 1;
            y = self.fit(y, h);
        }
        self.cells(row, &widths, stack, x, y, width, page, depth)
    }

    #[allow(clippy::too_many_arguments)]
    fn cells(&mut self, row: &[Cell], widths: &[f32], stack: bool, x: f32, y: f32, width: f32, page: [u8; 3], depth: usize) -> f32 {
        if stack {
            let mut y = y;
            for c in row.iter().filter(|c| !c.blocks.is_empty()) {
                let (slot, bottom) = self.cell(c, x, y, width, page, depth);
                if let Some(bg) = c.background {
                    self.behind(slot, Rect { x, y, w: width, h: bottom - y, color: bg });
                }
                y = bottom;
            }
            return y;
        }
        let mut cx = x;
        let mut bottom = y;
        let mut slots: Vec<(usize, f32, f32, [u8; 3])> = Vec::new();
        for (c, w) in row.iter().zip(widths) {
            let (slot, b) = self.cell(c, cx, y, *w, page, depth);
            bottom = bottom.max(b);
            if let Some(bg) = c.background {
                slots.push((slot, cx, *w, bg));
            }
            cx += w;
        }
        // Backgrounds are as tall as the row; later slots first, so earlier ones stay in place.
        for (slot, cx, w, bg) in slots.into_iter().rev() {
            self.behind(slot, Rect { x: cx, y, w, h: bottom - y, color: bg });
        }
        bottom
    }

    /// A cell `w` wide at (`x`, `y`): the item slot its content starts at and its bottom.
    fn cell(&mut self, c: &Cell, x: f32, y: f32, w: f32, page: [u8; 3], depth: usize) -> (usize, f32) {
        let page = c.background.unwrap_or(page);
        let slot = self.items.len();
        let inner = (w - 2.0 * CELL_PAD).max(1.0);
        let mut bottom = self.blocks(&c.blocks, x + CELL_PAD, y + CELL_PAD, inner, page, c.align, depth + 1) + CELL_PAD;
        for o in c.overlays.iter().take(16) {
            bottom = bottom.max(self.overlay(o, (x, y, w, bottom - y), page, depth));
        }
        (slot, bottom)
    }

    /// A box placed over `over` (x, y, w, h); where it ends at the bottom.
    fn overlay(&mut self, o: &Overlay, over: (f32, f32, f32, f32), page: [u8; 3], depth: usize) -> f32 {
        let (ox, oy, ow, oh) = over;
        let (dx, dy) = (o.x as f32 * PX, o.y as f32 * PX);
        let w = match o.width {
            Some(Width::Px(px)) => px as f32 * PX,
            Some(Width::Percent(pc)) => ow * f32::from(pc) / 100.0,
            None => self.natural(&o.blocks, depth + 1).map_or(ow - dx, |n| n + 2.0 * CELL_PAD + 1.0).min(ow - dx).max(18.0),
        }
        .clamp(1.0, ow.max(1.0));
        let left = if o.from_right { ox + ow - dx - w } else { ox + dx }.max(ox - w);
        let page = o.background.unwrap_or(page);
        let inner = (w - 2.0 * CELL_PAD).max(1.0);
        let top = if o.from_bottom {
            self.measuring += 1;
            let h = self.blocks(&o.blocks, left + CELL_PAD, 0.0, inner, page, Align::Left, depth + 1) + 2.0 * CELL_PAD;
            self.measuring -= 1;
            (oy + oh - dy - h).max(oy)
        } else {
            oy + dy
        };
        let slot = self.items.len();
        let bottom = self.blocks(&o.blocks, left + CELL_PAD, top + CELL_PAD, inner, page, Align::Left, depth + 1) + CELL_PAD;
        if let Some(bg) = o.background {
            self.behind(slot, Rect { x: left, y: top, w, h: bottom - top, color: bg });
        }
        bottom
    }
}

/// The widths of a row's cells in `width`: what cells (or their columns) ask for, the rest shared
/// by the others as their content compares (CSS's automatic table layout). `px` turns a
/// [`Width`] into points.
fn row_widths(
    row: &[Cell],
    columns: &[Option<Width>],
    natural: &[Option<f32>],
    measured: &[Option<f32>],
    width: f32,
    px: impl Fn(Width) -> f32,
) -> Vec<f32> {
    let mut col = 0usize;
    let mut asked: Vec<Option<f32>> = Vec::with_capacity(row.len());
    let mut wants: Vec<Option<f32>> = Vec::with_capacity(row.len());
    for (i, c) in row.iter().enumerate() {
        let span = usize::from(c.span.max(1));
        let from_columns = || {
            let parts = columns.get(col..col + span)?;
            (parts.len() == span && parts.iter().all(Option::is_some)).then(|| parts.iter().flatten().map(|w| px(*w)).sum::<f32>())
        };
        asked.push(c.width.map(&px).or_else(from_columns));
        let from_natural = || natural.get(col..col + span).filter(|p| p.len() == span)?.iter().copied().sum::<Option<f32>>();
        wants.push(from_natural().or_else(|| measured.get(i).copied().flatten()));
        col = col.saturating_add(span);
    }
    let fixed: f32 = asked.iter().flatten().sum();
    let free: Vec<usize> = (0..row.len()).filter(|i| asked.get(*i).is_some_and(Option::is_none)).collect();
    let known: Vec<f32> = free.iter().filter_map(|i| wants.get(*i).copied().flatten()).collect();
    let fallback = if known.is_empty() { MIN_COLUMN } else { known.iter().sum::<f32>() / known.len() as f32 };
    let want = |i: usize| wants.get(i).copied().flatten().unwrap_or(fallback).max(0.0) + 2.0 * CELL_PAD;
    let need = |i: usize| want(i).min(MIN_COLUMN);
    let remaining = width - fixed;
    let needs: f32 = free.iter().map(|i| need(*i)).sum();
    let mut widths: Vec<f32> = if fixed > width || (!free.is_empty() && remaining < needs) {
        let total: f32 = asked.iter().enumerate().map(|(i, a)| a.unwrap_or(need(i))).sum::<f32>().max(1.0);
        asked.iter().enumerate().map(|(i, a)| a.unwrap_or(need(i)) / total * width).collect()
    } else if !free.is_empty() {
        let total_want: f32 = free.iter().map(|i| want(*i)).sum::<f32>().max(1.0);
        if total_want <= remaining {
            asked.iter().enumerate().map(|(i, a)| a.unwrap_or(want(i) / total_want * remaining)).collect()
        } else {
            let growth: f32 = free.iter().map(|i| want(*i) - need(*i)).sum();
            asked
                .iter()
                .enumerate()
                .map(|(i, a)| a.unwrap_or(need(i) + if growth > 0.0 { (remaining - needs) * (want(i) - need(i)) / growth } else { 0.0 }))
                .collect()
        }
    } else {
        asked.iter().map(|a| a.unwrap_or(0.0)).collect()
    };
    let used: f32 = widths.iter().sum();
    if used < width
        && let Some(last) = widths.last_mut()
    {
        *last += width - used;
    }
    widths.iter_mut().for_each(|w| *w = w.max(1.0));
    widths
}

/// The width each column asks for: the first cell of one column that sets one, in any row.
fn column_widths(rows: &[Vec<Cell>]) -> Vec<Option<Width>> {
    const MAX_COLUMNS: usize = 64;
    let mut out: Vec<Option<Width>> = Vec::new();
    for row in rows {
        let mut col = 0usize;
        for c in row {
            let span = usize::from(c.span.max(1));
            if span == 1 && col < MAX_COLUMNS {
                if out.len() <= col {
                    out.resize(col + 1, None);
                }
                if let (Some(slot), Some(w)) = (out.get_mut(col), c.width)
                    && slot.is_none()
                {
                    *slot = Some(w);
                }
            }
            col = col.saturating_add(span);
        }
    }
    out
}

/// The widest content of each column, from the cells that span one; `None` where one can't be
/// measured.
fn column_naturals(rows: &[Vec<Cell>], measured: &[Vec<Option<f32>>]) -> Vec<Option<f32>> {
    const MAX_COLUMNS: usize = 64;
    let mut out: Vec<Option<f32>> = Vec::new();
    for (row, m) in rows.iter().zip(measured) {
        let mut col = 0usize;
        for (c, n) in row.iter().zip(m) {
            let span = usize::from(c.span.max(1));
            if span == 1 && col < MAX_COLUMNS {
                if out.len() <= col {
                    out.resize(col + 1, Some(0.0));
                }
                match (n, out.get_mut(col)) {
                    (Some(n), Some(Some(w))) => *w = w.max(*n),
                    (None, Some(slot)) => *slot = None,
                    _ => {}
                }
            }
            col = col.saturating_add(span);
        }
    }
    out
}

/// How wide a table is when nothing sizes it: its columns' widths (asked for in pixels, or their
/// content's with the cells' padding), or a row of spanning cells if that is more.
fn table_natural(rows: &[Vec<Cell>], columns: &[Option<Width>], natural: &[Option<f32>], measured: &[Vec<Option<f32>>]) -> Option<f32> {
    let of = |asked: Option<Width>, content: Option<f32>| match asked {
        Some(Width::Px(px)) => Some(px as f32 * PX),
        _ => content.map(|n| n + 2.0 * CELL_PAD),
    };
    let mut widest: f32 = natural.iter().enumerate().map(|(i, n)| of(columns.get(i).copied().flatten(), *n)).sum::<Option<f32>>()?;
    for (row, m) in rows.iter().zip(measured) {
        let w: f32 = row.iter().zip(m).map(|(c, n)| of(c.width, *n)).sum::<Option<f32>>()?;
        widest = widest.max(w);
    }
    Some(widest.min(4000.0))
}

/// The text of laid-out parts (what fonts must have characters for), pictures as their
/// descriptions.
pub fn text_of(blocks: &[Block], out: &mut String, depth: usize) {
    if depth > MAX_DEPTH || out.len() > 4 << 20 {
        return;
    }
    for b in blocks {
        match b {
            Block::Text(lines) => {
                for line in lines {
                    for sp in line {
                        out.push_str(&sp.text);
                    }
                    out.push('\n');
                }
            }
            Block::Table(t) => {
                for c in t.rows.iter().flatten() {
                    text_of(&c.blocks, out, depth + 1);
                    for o in &c.overlays {
                        text_of(&o.blocks, out, depth + 1);
                    }
                }
            }
        }
    }
}
