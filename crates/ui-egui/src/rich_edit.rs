//! The message editor, as in Outlook's message window: the text shows as it will be sent (bold,
//! italic, underline, sizes, colours, highlights, lists, indents, rules, pictures) with no marks,
//! formatted from the Message ribbon or with Outlook's shortcuts.
//!
//! The text is an ordinary text box over the message's plain text; its look comes from the
//! message's [`Styles`], which follow the text after each change, and a layouter that draws them.
//! Undo and redo keep the formatting with the text.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Color32, FontId, Galley, Key, Modifiers, Rect, RichText, Sense, Stroke, Ui, Vec2, vec2};
use mailcraft_engine::Compose;
use mailcraft_engine::mime::rich::{self, Align, List, PICTURE, Style, Styles, Wrap};

use crate::icons::{self, Icon};
use crate::theme::{self, Tokens};
use crate::{Action, Dialog};

/// Pixels per point.
const PX: f32 = 4.0 / 3.0;
/// One indent step (and the room for a bullet or number).
const INDENT: f32 = 28.0;
/// The size a picture takes until it's decoded (or when it can't be).
const PICTURE_FALLBACK: Vec2 = Vec2::splat(72.0);
/// The tallest a picture is drawn in the editor (a taller one is shown smaller; sent as set).
const PICTURE_TALLEST: f32 = 600.0;
/// The space between a picture and the text beside it.
const BESIDE_GAP: f32 = 12.0;
/// A picture the text sits beside takes at most this much of the width.
const BESIDE_SHARE: f32 = 0.6;
/// The handles on a selected picture's corners: their size.
const HANDLE: f32 = 8.0;
/// Most undo steps kept.
const UNDO_MAX: usize = 200;

/// Outlook's standard colours, and its highlighter's.
pub(crate) const COLORS: [[u8; 3]; 12] = [
    [0xC0, 0x00, 0x00],
    [0xFF, 0x00, 0x00],
    [0xFF, 0xC0, 0x00],
    [0xFF, 0xFF, 0x00],
    [0x92, 0xD0, 0x50],
    [0x00, 0xB0, 0x50],
    [0x00, 0xB0, 0xF0],
    [0x00, 0x70, 0xC0],
    [0x00, 0x20, 0x60],
    [0x70, 0x30, 0xA0],
    [0x00, 0x00, 0x00],
    [0x7F, 0x7F, 0x7F],
];
pub(crate) const HIGHLIGHTS: [[u8; 3]; 8] = [
    [0xFF, 0xFF, 0x00],
    [0x00, 0xFF, 0x00],
    [0x00, 0xFF, 0xFF],
    [0xFF, 0x00, 0xFF],
    [0x00, 0x00, 0xFF],
    [0xFF, 0x00, 0x00],
    [0xC0, 0xC0, 0xC0],
    [0x80, 0x80, 0x80],
];

/// What the editor remembers between frames, for one message.
pub(crate) struct Editor {
    pub id: u64,
    /// The look the next typed text takes, set from the ribbon with nothing selected.
    typing: Option<Style>,
    /// The selection (characters) at the end of the last frame.
    pub sel: Range<usize>,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    last_change: f64,
    /// The Insert link dialog, while open.
    link: Option<LinkForm>,
    /// Paste once the text has the keyboard back (asked from the right-click menu).
    paste: bool,
    /// The right-click menu was open last frame (closing it gives the text the keyboard back).
    menu_open: bool,
    /// The pictures decoded (`None`: not a picture, or not here yet), with their own size.
    pictures: HashMap<String, Option<Picture>>,
    /// The pictures drawn this frame: the character each stands for, and where it was drawn (a
    /// click on it selects it; a right-click on it is a right-click on that character).
    placed: Vec<(usize, Rect)>,
    /// The picture the open right-click menu is about.
    menu_picture: Option<usize>,
    /// A picture being resized by its corner: its character, and its size so far.
    resizing: Option<(usize, Vec2)>,
    /// Give the keyboard back to the text (after a ribbon button took the click), with this
    /// selection.
    pub(crate) refocus: Option<Range<usize>>,
}

/// A decoded picture: its texture (shrunk for the screen) and its own size in pixels.
struct Picture {
    tex: egui::TextureHandle,
    natural: Vec2,
}

/// What the layout knows of the pictures: their own sizes, and the one being resized.
#[derive(Clone, Default)]
struct Sizes {
    natural: HashMap<String, Vec2>,
    live: Option<(usize, Vec2)>,
}

impl Sizes {
    /// The size picture `at` (with style `s`) is drawn at: the one being dragged, else the one
    /// set (a width or height alone keeps its proportions), else its own.
    fn of(&self, at: usize, s: &Style) -> Vec2 {
        if let Some((i, size)) = self.live
            && i == at
        {
            return size;
        }
        let natural = s.picture.as_ref().and_then(|cid| self.natural.get(cid).copied()).unwrap_or(PICTURE_FALLBACK);
        sized(s, natural)
    }
}

/// The size a picture with its own size `natural` is shown and sent at, as its style says.
fn sized(s: &Style, natural: Vec2) -> Vec2 {
    let natural = Vec2::new(natural.x.max(1.0), natural.y.max(1.0));
    match (s.width, s.height) {
        (Some(w), Some(h)) => vec2(f32::from(w), f32::from(h)),
        (Some(w), None) => vec2(f32::from(w), f32::from(w) * natural.y / natural.x),
        (None, Some(h)) => vec2(f32::from(h) * natural.x / natural.y, f32::from(h)),
        (None, None) => natural,
    }
}

/// `size` shrunk to fit `max` (never enlarged).
fn fit(size: Vec2, max: Vec2) -> Vec2 {
    let size = Vec2::new(size.x.max(1.0), size.y.max(1.0));
    size * (max.x / size.x).min(max.y / size.y).clamp(0.01, 1.0)
}

/// Outlook's Insert Hyperlink, with what mail needs: a web page or file, or an email address.
#[derive(Clone, Debug, Default)]
struct LinkForm {
    email: bool,
    text: String,
    address: String,
    to: String,
    subject: String,
    /// The text the link goes on (the selection when the dialog opened).
    sel: Range<usize>,
    /// A link was there already (Remove link is offered).
    editing: bool,
    /// The selection spans lines: the link goes on it as it is (the text can't be retyped).
    multi: bool,
}

impl LinkForm {
    /// The dialog for the selection: its text, and the link already on it.
    fn open(c: &Compose, sel: Range<usize>) -> LinkForm {
        let chosen: String = c.body.chars().skip(sel.start).take(sel.end.saturating_sub(sel.start)).collect();
        let existing = c.styles.style(sel.start).link.filter(|_| !c.body.is_empty());
        let multi = chosen.contains('\n');
        let mut f = LinkForm { text: if multi { String::new() } else { chosen }, sel, editing: existing.is_some(), multi, ..Default::default() };
        match existing {
            Some(url) if url.to_ascii_lowercase().starts_with("mailto:") => {
                f.email = true;
                let rest = url.get(7..).unwrap_or_default();
                let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
                f.to = percent_decode(to);
                f.subject = query.split('&').find_map(|kv| kv.strip_prefix("subject=")).map(percent_decode).unwrap_or_default();
            }
            Some(url) => f.address = url,
            None => {}
        }
        f
    }

    /// The link's address: `https://` added to a bare web address, `file:///` to a path, and
    /// a `mailto:` with its subject.
    fn url(&self) -> Option<String> {
        if self.email {
            let to = self.to.trim();
            if to.is_empty() {
                return None;
            }
            let to = to.strip_prefix("mailto:").unwrap_or(to);
            let subject = self.subject.trim();
            return Some(if subject.is_empty() { format!("mailto:{to}") } else { format!("mailto:{to}?subject={}", percent_encode(subject)) });
        }
        let a = self.address.trim();
        if a.is_empty() || a == "https://" || a == "http://" {
            return None;
        }
        let lower = a.to_ascii_lowercase();
        let has_scheme = ["http://", "https://", "mailto:", "file:", "ftp://", "tel:"].iter().any(|p| lower.starts_with(p));
        let path = a.len() > 2 && a.as_bytes().get(1) == Some(&b':') && matches!(a.as_bytes().get(2), Some(b'\\' | b'/'));
        Some(if has_scheme {
            a.to_string()
        } else if path || a.starts_with("\\\\") {
            format!("file:///{}", a.replace('\\', "/").trim_start_matches('/'))
        } else {
            format!("https://{a}")
        })
    }
}

/// `%XX` escapes in a mailto address or subject, as text.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        let hex = |k: usize| b.get(k).and_then(|h| (*h as char).to_digit(16));
        match (c, hex(i + 1), hex(i + 2)) {
            (b'%', Some(h), Some(l)) => {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A subject as a mailto may carry it (RFC 6068): everything but letters, digits and `-._~`
/// escaped.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone)]
struct Snap {
    text: String,
    styles: Styles,
    cursor: usize,
}

impl Editor {
    /// Whether the picture `cid` was drawn from its bytes (tests).
    #[cfg(test)]
    pub(crate) fn has_picture(&self, cid: &str) -> bool {
        self.pictures.get(cid).is_some_and(Option::is_some)
    }

    /// Where the pictures were drawn last frame (tests).
    #[cfg(test)]
    pub(crate) fn placed_pictures(&self) -> Vec<Rect> {
        self.placed.iter().map(|(_, r)| *r).collect()
    }

    /// The pictures' own sizes, as decoded so far.
    fn sizes(&self) -> Sizes {
        Sizes { natural: self.pictures.iter().filter_map(|(cid, p)| p.as_ref().map(|p| (cid.clone(), p.natural))).collect(), live: self.resizing }
    }

    pub fn new(id: u64) -> Editor {
        Editor {
            id,
            typing: None,
            sel: 0..0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_change: 0.0,
            link: None,
            paste: false,
            menu_open: false,
            pictures: HashMap::new(),
            placed: Vec::new(),
            menu_picture: None,
            resizing: None,
            refocus: None,
        }
    }

    fn remember(&mut self, c: &Compose, cursor: usize) {
        self.undo.push(Snap { text: c.body.clone(), styles: c.styles.clone(), cursor });
        if self.undo.len() > UNDO_MAX {
            self.undo.remove(0);
        }
        self.redo.clear();
    }
}

/// The message text box's id.
/// The id of the signature being edited (the signature editor is a message editor of its own).
pub(crate) const SIGNATURE_ID: u64 = u64::MAX;

pub(crate) fn body_id(c: &Compose) -> egui::Id {
    egui::Id::unique(("compose-body", c.id))
}

fn selection(ctx: &egui::Context, id: egui::Id, n: usize) -> Option<Range<usize>> {
    let state = egui::TextEdit::load_state(ctx, id)?;
    let r = state.cursor.char_range()?.as_sorted_char_range();
    Some(r.start.0.min(n)..r.end.0.min(n))
}

fn set_selection(ctx: &egui::Context, id: egui::Id, r: Range<usize>) {
    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(r.start), CCursor::new(r.end))));
    state.store(ctx, id);
}

/// The look typing at `at` takes: the character before (or after, at a paragraph's start),
/// without a link the text doesn't continue.
fn typing_style(text: &str, styles: &Styles, at: usize) -> Style {
    let chars: Vec<char> = text.chars().collect();
    let before = at.checked_sub(1).filter(|i| chars.get(*i).is_some_and(|c| *c != '\n'));
    let i = before.or_else(|| chars.get(at).filter(|c| **c != '\n').map(|_| at));
    let Some(i) = i else { return Style::default() };
    let mut s = styles.style(i).for_typing();
    if s.link.is_some() && styles.style(at).link != s.link {
        s.link = None;
    }
    s
}

/// The pictures a selection means: those inside it, or with nothing selected the one the cursor
/// touches (just before or after it).
fn pictures_at(text: &str, styles: &Styles, sel: Range<usize>) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    let is_pic = |i: usize| chars.get(i).is_some_and(|c| *c == PICTURE) && styles.style(i).picture.is_some();
    if sel.is_empty() {
        return [sel.start.checked_sub(1), Some(sel.start)].into_iter().flatten().filter(|i| is_pic(*i)).take(1).collect();
    }
    (sel.start..sel.end.min(chars.len())).filter(|i| is_pic(*i)).collect()
}

/// A change the ribbon or a shortcut asks for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Op {
    Bold,
    Italic,
    Underline,
    Strike,
    Size(u8),
    Grow,
    Shrink,
    Color(Option<[u8; 3]>),
    Highlight(Option<[u8; 3]>),
    Bullets,
    Numbers,
    Align(Align),
    Indent,
    Outdent,
    Clear,
    Link(String),
    Unlink,
    /// Where the picture(s) at the selection sit: in line with the text, or beside it.
    Wrap(Option<Wrap>),
    /// The size the picture(s) at the selection are shown and sent at (`None`: their own).
    Resize(Option<[u16; 2]>),
}

/// Apply `op` to the selection (or to what's typed next, when nothing is selected).
pub(crate) fn apply(ed: &mut Editor, c: &mut Compose, op: Op) {
    c.styles.fit(&c.body);
    let n = c.body.chars().count();
    let sel = ed.sel.start.min(n)..ed.sel.end.min(n);
    ed.remember(c, sel.end);
    let collapsed = sel.is_empty();
    let base = ed.typing.clone().unwrap_or_else(|| typing_style(&c.body, &c.styles, sel.start));
    let toggled = |get: fn(&Style) -> bool| -> bool { if collapsed { !get(&base) } else { !rich::all(&c.body, &c.styles, sel.clone(), get) } };
    type F = Box<dyn Fn(&Style) -> Style>;
    let char_op: Option<F> = match op.clone() {
        Op::Bold => {
            let on = toggled(|s| s.bold);
            Some(Box::new(move |s| Style { bold: on, ..s.clone() }))
        }
        Op::Italic => {
            let on = toggled(|s| s.italic);
            Some(Box::new(move |s| Style { italic: on, ..s.clone() }))
        }
        Op::Underline => {
            let on = toggled(|s| s.underline);
            Some(Box::new(move |s| Style { underline: on, ..s.clone() }))
        }
        Op::Strike => {
            let on = toggled(|s| s.strike);
            Some(Box::new(move |s| Style { strike: on, ..s.clone() }))
        }
        Op::Size(z) => Some(Box::new(move |s| Style { size: (z != rich::DEFAULT_SIZE).then_some(z), ..s.clone() })),
        Op::Grow | Op::Shrink => {
            let up = op == Op::Grow;
            Some(Box::new(move |s| {
                let z = rich::step_size(s.size(), up);
                Style { size: (z != rich::DEFAULT_SIZE).then_some(z), ..s.clone() }
            }))
        }
        Op::Color(col) => Some(Box::new(move |s| Style { color: col, ..s.clone() })),
        Op::Highlight(col) => Some(Box::new(move |s| Style { highlight: col, ..s.clone() })),
        Op::Clear => Some(Box::new(|s| Style { picture: s.picture.clone(), wrap: s.wrap, width: s.width, height: s.height, ..Style::default() })),
        Op::Link(url) => {
            let url = url.trim().to_string();
            Some(Box::new(move |s| Style { link: Some(url.clone()), ..s.clone() }))
        }
        Op::Unlink => Some(Box::new(|s| Style { link: None, ..s.clone() })),
        _ => None,
    };
    if let Some(f) = char_op {
        if collapsed {
            ed.typing = Some(f(&base));
        } else {
            c.styles.restyle(&c.body, sel.clone(), f);
        }
    }
    if let Op::Wrap(wrap) = op {
        for i in pictures_at(&c.body, &c.styles, sel.clone()) {
            c.styles.restyle(&c.body, i..i + 1, |s| Style { wrap, ..s.clone() });
        }
        c.plain = false;
        return;
    }
    if let Op::Resize(size) = op {
        let (width, height) = match size {
            Some([w, h]) => (Some(w.clamp(rich::PICTURE_MIN_PX, rich::PICTURE_MAX_PX)), Some(h.clamp(rich::PICTURE_MIN_PX, rich::PICTURE_MAX_PX))),
            None => (None, None),
        };
        for i in pictures_at(&c.body, &c.styles, sel.clone()) {
            c.styles.restyle(&c.body, i..i + 1, |s| Style { width, height, ..s.clone() });
        }
        c.plain = false;
        return;
    }
    let (first, last) = rich::lines_of(&c.body, sel.clone());
    let all_list = |kind: List| (first..=last).all(|l| c.styles.para(l).list == kind);
    match op {
        Op::Bullets | Op::Numbers => {
            let kind = if op == Op::Bullets { List::Bullet } else { List::Number };
            let to = if all_list(kind) { List::None } else { kind };
            c.styles.relayout(&c.body, sel, |p| p.list = to);
        }
        Op::Align(a) => c.styles.relayout(&c.body, sel, |p| p.align = a),
        Op::Indent => c.styles.relayout(&c.body, sel, |p| p.indent = p.indent.saturating_add(1)),
        Op::Outdent => c.styles.relayout(&c.body, sel, |p| p.indent = p.indent.saturating_sub(1)),
        Op::Clear => c.styles.relayout(&c.body, sel, |p| *p = rich::Para { rule: p.rule, ..Default::default() }),
        _ => {}
    }
    c.plain = false;
}

/// Put `text` in place of the characters in `range`, with style `style`.
fn replace(ed: &mut Editor, c: &mut Compose, range: Range<usize>, text: &str, style: Style) {
    ed.remember(c, range.end);
    let old = c.body.clone();
    let byte = |at: usize| old.char_indices().nth(at).map_or(old.len(), |(b, _)| b);
    let (a, b) = (byte(range.start), byte(range.end.max(range.start)));
    c.body.replace_range(a..b, text);
    let n = text.chars().count();
    c.styles.edit_at(&old, &c.body, &style, Some(range.start + n));
    ed.sel = range.start + n..range.start + n;
}

fn undo(ed: &mut Editor, c: &mut Compose, ctx: &egui::Context, redo: bool) {
    let (from, to) = if redo { (&mut ed.redo, &mut ed.undo) } else { (&mut ed.undo, &mut ed.redo) };
    let Some(s) = from.pop() else { return };
    to.push(Snap { text: c.body.clone(), styles: c.styles.clone(), cursor: ed.sel.end });
    c.body = s.text;
    c.styles = s.styles;
    let at = s.cursor.min(c.body.chars().count());
    ed.sel = at..at;
    ed.typing = None;
    set_selection(ctx, body_id(c), at..at);
}

/// Outlook's shortcuts in the text, taken before the text box sees them.
fn keys(ui: &mut Ui, ed: &mut Editor, c: &mut Compose, acts: &mut Vec<Action>) {
    let ctx = ui.ctx().clone();
    let id = body_id(c);
    let n = c.body.chars().count();
    if let Some(sel) = selection(&ctx, id, n) {
        ed.sel = sel;
    }
    let cmd = Modifiers::COMMAND;
    let cs = Modifiers::COMMAND | Modifiers::SHIFT;
    let mut ops = Vec::new();
    let (mut undo_k, mut redo_k, mut link_k, mut book_k) = (false, false, false, false);
    // Lists: Enter on an empty item ends the list, Backspace at an item's start takes its bullet
    // away, Tab and Shift+Tab move it in and out.
    let line = rich::lines_of(&c.body, ed.sel.start..ed.sel.start).0;
    let para = c.styles.para(line);
    let line_start = c.body.split('\n').take(line).map(|l| l.chars().count() + 1).sum::<usize>();
    let line_empty = c.body.split('\n').nth(line).is_some_and(str::is_empty);
    let at_start = ed.sel.is_empty() && ed.sel.start == line_start;
    let listed = para.list != List::None;
    ui.input_mut(|i| {
        // With Shift first, so Ctrl+Shift+Z isn't taken as Ctrl+Z.
        for (m, k, op) in [
            (cs, Key::L, Op::Bullets),
            (cs, Key::Period, Op::Grow),
            (cs, Key::Comma, Op::Shrink),
            (cs, Key::T, Op::Outdent),
            (cs, Key::Z, Op::Clear),
            (cmd, Key::B, Op::Bold),
            (cmd, Key::I, Op::Italic),
            (cmd, Key::U, Op::Underline),
            (cmd, Key::CloseBracket, Op::Grow),
            (cmd, Key::OpenBracket, Op::Shrink),
            (cmd, Key::L, Op::Align(Align::Left)),
            (cmd, Key::E, Op::Align(Align::Center)),
            (cmd, Key::R, Op::Align(Align::Right)),
            (cmd, Key::T, Op::Indent),
            (cmd, Key::Space, Op::Clear),
        ] {
            if i.consume_key(m, k) {
                ops.push(op);
            }
        }
        book_k = i.consume_key(cs, Key::B);
        undo_k = i.consume_key(cmd, Key::Z);
        redo_k = i.consume_key(cmd, Key::Y);
        link_k = i.consume_key(cmd, Key::K);
        if listed && line_empty && ed.sel.is_empty() && i.consume_key(Modifiers::NONE, Key::Enter) {
            ops.push(if para.list == List::Bullet { Op::Bullets } else { Op::Numbers });
        }
        if at_start && (listed || para.indent > 0) && i.consume_key(Modifiers::NONE, Key::Backspace) {
            ops.push(if listed { if para.list == List::Bullet { Op::Bullets } else { Op::Numbers } } else { Op::Outdent });
        }
        if listed && i.consume_key(Modifiers::SHIFT, Key::Tab) {
            ops.push(Op::Outdent);
        }
        if listed && i.consume_key(Modifiers::NONE, Key::Tab) {
            ops.push(Op::Indent);
        }
    });
    for op in ops {
        apply(ed, c, op);
    }
    if undo_k || redo_k {
        undo(ed, c, &ctx, redo_k);
    }
    if link_k {
        ed.link = Some(LinkForm::open(c, ed.sel.clone()));
    }
    if book_k && c.id != SIGNATURE_ID {
        acts.push(Action::Dialog(Dialog::select_names(c, "to")));
    }
}

/// Paste what another program put on the clipboard as HTML (a signature copied from Outlook,
/// part of a web page) with its look, in place of the selection. `false` when there is none,
/// or it holds no text (a picture alone is pasted as a picture).
fn paste_formatted(ui: &mut Ui, ed: &mut Editor, c: &mut Compose, id: egui::Id) -> bool {
    let Some(html) = crate::clipboard_html() else { return false };
    // The signature editor keeps the pictures the HTML carries itself (`data:` addresses), under
    // names of its own.
    let signature = c.id == SIGNATURE_ID;
    let taken: Vec<String> = c.inline.iter().map(|p| p.cid.clone()).collect();
    let mut n = 0;
    let mut cid_for = || {
        if !signature {
            return None;
        }
        loop {
            n += 1;
            let cid = format!("pasted-{n}@mailcraft");
            if !taken.contains(&cid) {
                return Some(cid);
            }
        }
    };
    let (add, add_styles, pictures) = mailcraft_engine::mime::pasted_html_with_pictures(&html, &mut cid_for);
    if add.trim().is_empty() && pictures.is_empty() {
        return false;
    }
    c.inline.extend(pictures);
    ui.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Paste(_))));
    c.styles.fit(&c.body);
    ed.undo.push(Snap { text: c.body.clone(), styles: c.styles.clone(), cursor: ed.sel.start });
    if ed.undo.len() > UNDO_MAX {
        ed.undo.remove(0);
    }
    ed.redo.clear();
    let n = c.body.chars().count();
    let sel = ed.sel.start.min(n)..ed.sel.end.min(n);
    rich::splice(&mut c.body, &mut c.styles, sel.clone(), &add, &add_styles);
    let end = sel.start + add.chars().count();
    set_selection(ui.ctx(), id, end..end);
    ed.sel = end..end;
    ed.typing = None;
    ui.ctx().request_repaint();
    true
}

/// A look as text formatting (the person's colours adjusted to stay readable on the page).
fn format(s: &Style, t: &Tokens) -> TextFormat {
    let px = f32::from(s.size()) * PX;
    let font = if s.mono {
        FontId::monospace(px * 0.92)
    } else if s.bold {
        theme::bold(px)
    } else {
        theme::regular(px)
    };
    let (mut color, background) = theme::mail_colors(s.color, s.highlight, t, t.text);
    if s.link.is_some() && s.color.is_none() {
        color = t.accent;
    }
    let line = Stroke::new((px / 14.0).max(1.0), color);
    TextFormat {
        font_id: font,
        color,
        background: background.unwrap_or(Color32::TRANSPARENT),
        italics: s.italic,
        underline: if s.underline || s.link.is_some() { line } else { Stroke::NONE },
        strikethrough: if s.strike { line } else { Stroke::NONE },
        ..Default::default()
    }
}

/// Room for a picture in the line: an invisible character as wide as the picture, on a row as
/// tall as it (the text next to it sits at the bottom, as beside a picture in a web page).
fn picture_format(size: Vec2) -> TextFormat {
    TextFormat {
        font_id: theme::regular(14.0),
        color: Color32::TRANSPARENT,
        extra_letter_spacing: size.x.max(1.0),
        line_height: Some(size.y.max(4.0) + 4.0),
        ..Default::default()
    }
}

/// The height of the row of a picture the text sits beside, when nothing else is on it: next to
/// nothing, so the column's first line starts at the picture's top, as it's sent.
const MARK_ROW: f32 = 3.0;

/// The widest a picture in the line is drawn: the room left of `wrap` after the indent.
fn inline_max(wrap: f32, lead: f32) -> Vec2 {
    vec2((wrap - lead - 4.0).max(24.0), PICTURE_TALLEST)
}

/// The text laid out with its styles; `shift` moves a paragraph right (alignment).
fn job(text: &str, st: &Styles, plain: bool, t: &Tokens, wrap: f32, shift: &[f32], sizes: &Sizes) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    let plain_fmt = TextFormat { font_id: theme::regular(14.5), color: t.text, ..Default::default() };
    let mut i = 0usize;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            let f = if plain { plain_fmt.clone() } else { format(&st.style(i.saturating_sub(1)), t) };
            job.append("\n", 0.0, f);
            i += 1;
        }
        let p = st.para(n);
        let para_lead = if plain { 0.0 } else { f32::from(p.indent) * INDENT + if p.list == List::None { 0.0 } else { INDENT } };
        let mut lead = para_lead + shift.get(n).copied().unwrap_or(0.0);
        let mut run_start = 0usize;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let alone = chars.len() == 1;
        for k in 0..chars.len() {
            let cur = st.chars.get(i + k).copied();
            let next = st.chars.get(i + k + 1).copied();
            let pic = chars.get(k).is_some_and(|(_, ch)| *ch == PICTURE) || chars.get(k + 1).is_some_and(|(_, ch)| *ch == PICTURE);
            if k + 1 == chars.len() || cur != next || pic {
                let from = chars.get(run_start).map_or(line.len(), |(b, _)| *b);
                let to = chars.get(k + 1).map_or(line.len(), |(b, _)| *b);
                let piece = line.get(from..to).unwrap_or("");
                let f = if plain {
                    plain_fmt.clone()
                } else if piece.starts_with(PICTURE) && st.style(i + run_start).picture.is_some() {
                    let st0 = st.style(i + run_start);
                    if st0.wrap.is_some() {
                        // Beside the text, the picture is drawn in the margin: its character
                        // takes no room, and its own paragraph next to none.
                        let mut f = TextFormat { color: Color32::TRANSPARENT, ..format(&st0, t) };
                        f.font_id.size = 1.0;
                        if alone {
                            f.line_height = Some(MARK_ROW);
                        }
                        f
                    } else {
                        picture_format(fit(sizes.of(i + run_start, &st0), inline_max(wrap, para_lead)))
                    }
                } else {
                    format(&st.style(i + run_start), t)
                };
                job.append(piece, lead, f);
                lead = 0.0;
                run_start = k + 1;
            }
        }
        if chars.is_empty() && lead > 0.0 {
            let f = if plain { plain_fmt.clone() } else { format(&st.style(i), t) };
            job.append("", lead, f);
        }
        i += chars.len();
    }
    job
}

/// egui puts an empty line at the left edge whatever its indent, so the caret on an empty
/// bulleted or indented line sat before its bullet: such rows are moved to their indent.
fn indent_empty_rows(g: Arc<Galley>, st: &Styles, plain: bool) -> Arc<Galley> {
    if plain {
        return g;
    }
    let lead = |n: usize| {
        let p = st.para(n);
        f32::from(p.indent) * INDENT + if p.list == List::None { 0.0 } else { INDENT }
    };
    let mut para = 0usize;
    let mut moves = Vec::new();
    for (i, r) in g.rows.iter().enumerate() {
        if r.row.glyphs.is_empty() && lead(para) > 0.0 {
            moves.push((i, lead(para)));
        }
        if r.ends_with_newline {
            para += 1;
        }
    }
    if moves.is_empty() {
        return g;
    }
    let mut out = (*g).clone();
    for (i, x) in moves {
        if let Some(r) = out.rows.get_mut(i) {
            r.pos.x = x;
        }
    }
    Arc::new(out)
}

/// A picture the text sits beside, as laid out: the column's paragraphs, which side, and the
/// size the picture is drawn at.
#[derive(Clone, Debug)]
struct Column {
    at: usize,
    paras: Range<usize>,
    side: Wrap,
    size: Vec2,
}

/// The pictures the text sits beside and their columns ([`rich::beside_blocks`]), with the
/// size each picture is drawn at: as set, no wider than its share of `wrap`.
fn columns(text: &str, st: &Styles, wrap: f32, sizes: &Sizes) -> Vec<Column> {
    rich::beside_blocks(text, st)
        .into_iter()
        .map(|b| {
            let size = fit(sizes.of(b.at, &st.style(b.at)), vec2((wrap * BESIDE_SHARE).max(24.0), PICTURE_TALLEST));
            Column { at: b.at, paras: b.paras, side: b.side, size }
        })
        .collect()
}

/// The room (the picture's width and the gap) and side for each paragraph that's in a column.
fn beside_of(cols: &[Column], paras: usize) -> Vec<Option<(f32, Wrap)>> {
    let mut out: Vec<Option<(f32, Wrap)>> = vec![None; paras];
    for c in cols {
        for n in c.paras.clone() {
            if let Some(o) = out.get_mut(n) {
                *o = Some((c.size.x + BESIDE_GAP, c.side));
            }
        }
    }
    out
}

/// Lay the text out; centred and right-aligned paragraphs that fit on one line are moved over,
/// and the columns beside pictures make room for them: their rows move right of a picture on
/// the left, and what follows a column shorter than its picture moves down below it.
fn layout(ui: &Ui, text: &str, st: &Styles, plain: bool, t: &Tokens, wrap: f32, sizes: &Sizes) -> Arc<Galley> {
    let (g, cols) = layout_aligned(ui, text, st, plain, t, wrap, sizes);
    let g = indent_empty_rows(g, st, plain);
    if cols.is_empty() {
        return g;
    }
    let mut out = (*g).clone();
    let mut n = 0usize;
    // How far the rows are pushed down so far, and the column being passed through: which,
    // the picture's bottom, and the last row's.
    let mut dy = 0.0f32;
    let mut open: Option<(usize, f32, f32)> = None;
    let mut bottom = 0.0f32;
    for r in &mut out.rows {
        let col = cols.iter().find(|c| c.paras.contains(&n));
        if let Some((at, pic, last)) = open
            && col.is_none_or(|c| c.at != at)
        {
            // Past the column: this row and the rest go below the picture.
            if pic > last {
                dy += pic - last;
            }
            open = None;
        }
        r.pos.y += dy;
        if let Some(c) = col {
            if open.is_none() {
                open = Some((c.at, r.pos.y + c.size.y + 6.0, r.pos.y));
            }
            if c.side == Wrap::Left {
                r.pos.x += c.size.x + BESIDE_GAP;
            }
            if let Some((_, _, last)) = open.as_mut() {
                *last = r.rect().bottom();
            }
        }
        bottom = bottom.max(r.rect().bottom());
        if r.ends_with_newline {
            n += 1;
        }
    }
    if let Some((_, pic, _)) = open {
        bottom = bottom.max(pic);
    }
    // The text box paints the galley at its left minus the rect's: the rect starts at 0, or
    // the rows moved right of a picture would be drawn under it.
    let rows = out.rows.iter().fold(Rect::NOTHING, |acc, r| acc.union(r.rect()));
    out.rect = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(rows.max.x.max(0.0), rows.max.y.max(bottom)));
    Arc::new(out)
}

fn layout_aligned(ui: &Ui, text: &str, st: &Styles, plain: bool, t: &Tokens, wrap: f32, sizes: &Sizes) -> (Arc<Galley>, Vec<Column>) {
    let mut first = ui.painter().layout_job(job(text, st, plain, t, wrap, &[], sizes));
    let cols = if plain { Vec::new() } else { columns(text, st, wrap, sizes) };
    let beside = beside_of(&cols, st.paras.len().max(1));
    // Text beside a picture has less room: laid out again, narrower, when a line would run
    // under the picture.
    let mut widths = vec![wrap; st.paras.len().max(1)];
    if !cols.is_empty() {
        let margin = beside.iter().flatten().map(|(m, _)| *m).fold(0.0f32, f32::max).min(wrap * BESIDE_SHARE);
        let mut n = 0usize;
        let mut overflow = false;
        for r in &first.rows {
            if beside.get(n).is_some_and(Option::is_some) && r.rect().max.x > wrap - margin {
                overflow = true;
            }
            if r.ends_with_newline {
                n += 1;
            }
        }
        if overflow {
            first = ui.painter().layout_job(job(text, st, plain, t, wrap - margin, &[], sizes));
        }
        for (n, w) in widths.iter_mut().enumerate() {
            if let Some(Some((m, _))) = beside.get(n) {
                *w = if overflow { wrap - margin } else { wrap - m };
            } else if overflow {
                *w = wrap - margin;
            }
        }
    }
    // A right-to-left paragraph left as it was sits against the right edge.
    let lines: Vec<&str> = text.split('\n').collect();
    let align_of = |n: usize, a: Align| {
        if a == Align::Left && lines.get(n).is_some_and(|l| crate::bidi::has_rtl(l) && crate::bidi::paragraph_rtl(l)) { Align::Right } else { a }
    };
    if plain || st.paras.iter().enumerate().all(|(n, p)| align_of(n, p.align) == Align::Left) {
        return (first, cols);
    }
    let mut shift = vec![0.0; st.paras.len()];
    let mut row = 0;
    for (n, p) in st.paras.iter().enumerate() {
        // The rows this paragraph takes.
        let start = row;
        while let Some(r) = first.rows.get(row) {
            row += 1;
            if r.ends_with_newline {
                break;
            }
        }
        let align = align_of(n, p.align);
        if align != Align::Left
            && row == start + 1
            && let Some(r) = first.rows.get(start)
        {
            let width = widths.get(n).copied().unwrap_or(wrap);
            let room = (width - r.rect().max.x).max(0.0);
            if let Some(s) = shift.get_mut(n) {
                *s = if align == Align::Center { room / 2.0 } else { room };
            }
        }
    }
    let max_width = widths.iter().copied().fold(wrap, f32::min);
    (ui.painter().layout_job(job(text, st, plain, t, max_width, &shift, sizes)), cols)
}

/// The message text: the text box, then bullets and numbers, rules and pictures drawn over it.
pub(crate) fn body(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose, start_at: Option<usize>, acts: &mut Vec<Action>) {
    let id = body_id(c);
    c.styles.fit(&c.body);
    // Back from the ribbon: the cursor (or selection) where it was, typing in the text again.
    // Not on the frame of the click itself, when the text box gives the keyboard up because
    // it was clicked elsewhere.
    if ed.refocus.is_some() {
        if ui.input(|i| i.pointer.any_pressed() || i.pointer.any_released()) {
            ui.ctx().request_repaint();
        } else if let Some(sel) = ed.refocus.take() {
            ui.memory_mut(|m| m.request_focus(id));
            set_selection(ui.ctx(), id, sel.clone());
            ed.sel = sel;
            // Paste from the right-click menu: the system sends the text to the text box now
            // that it has the keyboard.
            if std::mem::take(&mut ed.paste) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            }
        }
    }
    if ui.memory(|m| m.has_focus(id)) {
        keys(ui, ed, c, acts);
        if !c.plain && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))) {
            paste_formatted(ui, ed, c, id);
        }
    }
    let before = c.body.clone();
    let styles = c.styles.clone();
    let plain = c.plain;
    let tokens = *t;
    let sizes = ed.sizes();
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| layout(ui, buf.as_str(), &styles, plain, &tokens, wrap, &sizes);
    // With the original drawn under it, the text takes the room it needs (a few lines at
    // least), so the original is right there; alone, it fills the pane.
    let height = if c.quoted.is_some() { 120.0 } else { ui.available_height().max(220.0) };
    // A right-click inside the selection keeps it (egui's text box moves the cursor on any
    // press); one outside it moves the cursor there, as Outlook does.
    let kept = ed.sel.clone();
    let right_press = ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary));
    // The selection is drawn behind the text while the text box doesn't have the keyboard
    // (a menu, a dialog or the ribbon has it), so what's selected stays plain to see.
    let behind = ui.painter().add(egui::Shape::Noop);
    // No hint over a first line that's a list item or indented (its bullet is drawn there).
    let p0 = c.styles.para(0);
    let hint = match (p0.list == List::None && p0.indent == 0, c.id == SIGNATURE_ID) {
        (false, _) => "",
        (true, true) => tl!("Write your signature, or paste it…"),
        (true, false) => tl!("Write your message…"),
    };
    let out = egui::TextEdit::multiline(&mut c.body)
        .id(id)
        .frame(egui::Frame::NONE)
        .hint_text(hint)
        .lock_focus(true)
        .desired_width(f32::INFINITY)
        .min_size(vec2(ui.available_width(), height))
        .layouter(&mut layouter)
        .show(ui);
    let mut cursor = out.cursor_range.map(|r| {
        let r = r.as_sorted_char_range();
        r.start.0..r.end.0
    });
    if right_press
        && !kept.is_empty()
        && let Some(p) = out.response.interact_pointer_pos().or_else(|| out.response.hover_pos())
    {
        let at = out.galley.cursor_from_pos(p - out.galley_pos).index.0;
        if (kept.start..=kept.end).contains(&at) {
            set_selection(ui.ctx(), id, kept.clone());
            cursor = Some(kept.clone());
        }
    }
    // A click on a picture selects it (as in Word): its handles show, and Delete removes it.
    if !c.plain
        && out.response.clicked()
        && let Some(p) = out.response.interact_pointer_pos()
        && let Some((i, _)) = ed.placed.iter().find(|(_, r)| r.contains(p))
    {
        set_selection(ui.ctx(), id, *i..*i + 1);
        cursor = Some(*i..*i + 1);
    }
    if c.body != before {
        // The look of what was typed: the ribbon's choice, else the text's at the cursor.
        let typing = ed.typing.take().unwrap_or_else(|| typing_style(&before, &styles, ed.sel.start));
        let now = ui.input(|i| i.time);
        let breaks = c.body.matches('\n').count() != before.matches('\n').count();
        if now - ed.last_change > 1.0 || breaks || ed.undo.is_empty() {
            ed.undo.push(Snap { text: before.clone(), styles: styles.clone(), cursor: ed.sel.start });
            if ed.undo.len() > UNDO_MAX {
                ed.undo.remove(0);
            }
            ed.redo.clear();
        }
        ed.last_change = now;
        c.styles.edit_at(&before, &c.body, &typing, cursor.as_ref().map(|r| r.end));
        ui.ctx().request_repaint();
    } else if cursor.as_ref().is_some_and(|r| *r != ed.sel) {
        // The cursor moved: what the ribbon set for typing no longer applies.
        ed.typing = None;
    }
    if let Some(r) = cursor {
        ed.sel = r;
    }
    if let Some(at) = start_at {
        out.response.request_focus();
        set_selection(ui.ctx(), id, at..at);
        ed.sel = at..at;
    }
    if !c.plain {
        let focused = out.response.has_focus();
        decorate(ui, t, ed, c, &out.galley, out.galley_pos, focused);
    }
    if !out.response.has_focus() && !ed.sel.is_empty() {
        let color = ui.visuals().selection.bg_fill.gamma_multiply(0.45);
        let shapes: Vec<egui::Shape> = selection_rects(&out.galley, ed.sel.clone())
            .into_iter()
            .map(|r| egui::Shape::rect_filled(r.translate(out.galley_pos.to_vec2()), 0.0, color))
            .collect();
        ui.painter().set(behind, egui::Shape::Vec(shapes));
    }
    text_menu(&out.response, ed, c, acts);
}

/// The rectangles a selection covers, one per row (galley coordinates).
fn selection_rects(g: &Galley, sel: Range<usize>) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    // Long selections are drawn in full; past this many characters only their start is.
    for i in sel.start..sel.end.min(sel.start + 20_000) {
        let a = g.pos_from_cursor(CCursor::new(i));
        let b = g.pos_from_cursor(CCursor::new(i + 1));
        // A line's end: the selection runs a little past the text, as text boxes show it.
        let right = if (b.min.y - a.min.y).abs() < 0.5 { b.min.x } else { a.min.x + 5.0 };
        let r = Rect::from_min_max(egui::pos2(a.min.x, a.min.y), egui::pos2(right.max(a.min.x), a.max.y));
        match out.last_mut() {
            Some(last) if (last.min.y - r.min.y).abs() < 0.5 => *last = last.union(r),
            _ => out.push(r),
        }
    }
    out
}

/// The right-click menu on the text, as Outlook's: Cut, Copy, Paste and Select all; the link
/// (insert, edit, open, remove); and clearing the formatting.
fn text_menu(resp: &egui::Response, ed: &mut Editor, c: &mut Compose, acts: &mut Vec<Action>) {
    let n = c.body.chars().count();
    let sel = ed.sel.start.min(n)..ed.sel.end.min(n);
    let chosen: String = c.body.chars().skip(sel.start).take(sel.end - sel.start).collect();
    let link = c.styles.style(sel.start).link.filter(|_| n > 0);
    // A right-click on a picture is one on its character (remembered while the menu is open:
    // the text box puts its cursor where the click landed).
    if resp.secondary_clicked() {
        ed.menu_picture = resp.interact_pointer_pos().and_then(|pos| ed.placed.iter().find(|(_, r)| r.contains(pos)).map(|(i, _)| *i));
        if let Some(i) = ed.menu_picture {
            ed.sel = i..i + 1;
            set_selection(&resp.ctx, resp.id, i..i + 1);
        }
    }
    let sel = ed.menu_picture.map_or(sel, |i| i..i + 1);
    let pictures = pictures_at(&c.body, &c.styles, sel.clone());
    let wrap_now = pictures.first().map(|i| c.styles.style(*i).wrap);
    let shown = resp.context_menu(|ui| {
        ui.set_min_width(190.0);
        let item = |ui: &mut Ui, label: &str, keys: &str, enabled: bool| -> bool {
            let b = egui::Button::new(tl!(label)).shortcut_text(keys);
            let clicked = ui.add_enabled(enabled, b).clicked();
            if clicked {
                ui.close();
            }
            clicked
        };
        let some = !sel.is_empty();
        if item(ui, "Cut", "Ctrl+X", some && !c.sending) {
            ui.ctx().copy_text(chosen.clone());
            let style = typing_style(&c.body, &c.styles, sel.start);
            replace(ed, c, sel.clone(), "", style);
            ed.refocus = Some(ed.sel.clone());
        }
        if item(ui, "Copy", "Ctrl+C", some) {
            ui.ctx().copy_text(chosen.clone());
            ed.refocus = Some(sel.clone());
        }
        if item(ui, "Paste", "Ctrl+V", !c.sending) {
            ed.paste = true;
            ed.refocus = Some(sel.clone());
        }
        ui.separator();
        if item(ui, "Select all", "Ctrl+A", n > 0) {
            ed.refocus = Some(0..n);
        }
        if !c.plain {
            ui.separator();
            if item(ui, if link.is_some() { "Edit link…" } else { "Link…" }, "Ctrl+K", !c.sending) {
                ed.link = Some(LinkForm::open(c, sel.clone()));
            }
            if let Some(url) = &link {
                if item(ui, "Open link", "", true) {
                    acts.push(Action::OpenUrl(url.clone()));
                }
                if item(ui, "Remove link", "", !c.sending) {
                    apply(ed, c, Op::Unlink);
                    ed.refocus = Some(sel.clone());
                }
            }
            ui.separator();
            if item(ui, "Clear formatting", "Ctrl+Space", some && !c.sending) {
                apply(ed, c, Op::Clear);
                ed.refocus = Some(sel.clone());
            }
            // Where a picture sits: in the line, or with the text flowing beside it (Word's
            // "wrap text").
            if !pictures.is_empty() && !c.sending {
                ui.separator();
                ui.menu_button(tl!("Picture"), |ui| {
                    for (label, wrap) in
                        [("In line with text", None), ("Left of the text", Some(Wrap::Left)), ("Right of the text", Some(Wrap::Right))]
                    {
                        if ui.radio(wrap_now == Some(wrap), tl!(label)).clicked() {
                            ed.sel = sel.clone();
                            apply(ed, c, Op::Wrap(wrap));
                            ed.refocus = Some(ed.sel.clone());
                            ui.close();
                        }
                    }
                    ui.separator();
                    // Its size: a step smaller or larger, or as it is (also by dragging its
                    // corner handles).
                    let shown = pictures.first().map(|i| ed.sizes().of(*i, &c.styles.style(*i))).unwrap_or(PICTURE_FALLBACK);
                    let sized_now = pictures.first().is_some_and(|i| {
                        let st = c.styles.style(*i);
                        st.width.is_some() || st.height.is_some()
                    });
                    let px = |v: f32| {
                        u16::try_from((v.round().clamp(f32::from(rich::PICTURE_MIN_PX), f32::from(rich::PICTURE_MAX_PX))) as u32)
                            .unwrap_or(rich::PICTURE_MAX_PX)
                    };
                    for (label, factor) in [("Smaller", 0.8), ("Larger", 1.25)] {
                        if item(ui, label, "", true) {
                            ed.sel = sel.clone();
                            apply(ed, c, Op::Resize(Some([px(shown.x * factor), px(shown.y * factor)])));
                            ed.refocus = Some(ed.sel.clone());
                        }
                    }
                    if item(ui, "Original size", "", sized_now) {
                        ed.sel = sel.clone();
                        apply(ed, c, Op::Resize(None));
                        ed.refocus = Some(ed.sel.clone());
                    }
                    ui.label(RichText::new(format!("{} × {} px", shown.x.round(), shown.y.round())).color(ui.visuals().weak_text_color()).small());
                });
            }
        }
    });
    // Closed without a choice (Escape, a click elsewhere in the window): the text gets the
    // keyboard back, with what was selected.
    let open = shown.is_some();
    if ed.menu_open && !open && ed.refocus.is_none() && ed.link.is_none() && !resp.ctx.input(|i| i.pointer.any_pressed() && !resp.hovered()) {
        ed.refocus = Some(sel.clone());
    }
    if !open {
        ed.menu_picture = None;
    }
    ed.menu_open = open;
}

/// Bullets and numbers, rules and pictures (with their handles when selected).
fn decorate(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose, g: &Galley, origin: egui::Pos2, focused: bool) {
    let painter = ui.painter().clone();
    ed.placed.clear();
    let at = |i: usize| -> Rect { g.pos_from_cursor(CCursor::new(i)).translate(origin.to_vec2()) };
    let mut i = 0usize;
    let mut numbers: Vec<u32> = vec![0; 16];
    let right = ui.min_rect().right().max(origin.x + g.size().x);
    let wrap = g.job.wrap.max_width;
    let cols = columns(&c.body, &c.styles, wrap, &ed.sizes());
    let mut resized: Option<(usize, Vec2)> = None;
    let body = c.body.clone();
    for (n, line) in body.split('\n').enumerate() {
        let p = c.styles.para(n);
        let r = at(i);
        if p.rule {
            let y = r.center().y;
            painter.line_segment([egui::pos2(origin.x, y), egui::pos2(right, y)], Stroke::new(1.0, t.border));
        }
        let depth = usize::from(p.indent).min(15);
        match p.list {
            List::None => numbers.iter_mut().for_each(|x| *x = 0),
            List::Bullet | List::Number => {
                for x in numbers.iter_mut().skip(depth + 1) {
                    *x = 0;
                }
                let style = c.styles.style(i);
                let px = f32::from(style.size()) * PX;
                let mark = if p.list == List::Bullet {
                    if depth % 2 == 0 { "•".to_string() } else { "◦".to_string() }
                } else {
                    let k = numbers.get_mut(depth).map(|x| {
                        *x += 1;
                        *x
                    });
                    format!("{}.", k.unwrap_or(1))
                };
                let x = origin.x + f32::from(p.indent) * INDENT + INDENT - 8.0;
                painter.text(egui::pos2(x, r.center().y), egui::Align2::RIGHT_CENTER, mark, theme::regular(px), t.text);
            }
        }
        if p.list != List::Number
            && let Some(x) = numbers.get_mut(depth)
        {
            *x = 0;
        }
        for (k, ch) in line.chars().enumerate() {
            if ch != PICTURE {
                continue;
            }
            let style = c.styles.style(i + k);
            let Some(cid) = style.picture.clone() else { continue };
            // A picture not here yet (a web picture being downloaded) is looked for again.
            let pic = ed.pictures.entry(cid.clone()).or_insert(None);
            if pic.is_none()
                && let Some(p) = c.inline.iter().find(|p| p.cid == cid)
            {
                *pic = crate::reading::decode(ui.ctx(), &format!("compose-{cid}"), &p.data, 1024)
                    .map(|tex| Picture { natural: natural_size(&p.data).unwrap_or_else(|| tex.size_vec2()), tex });
            }
            let tex = pic.as_ref().map(|p| p.tex.id());
            let sizes = ed.sizes();
            let (a, b) = (at(i + k), at(i + k + 1));
            let shown = match style.wrap {
                Some(side) => {
                    // Beside the text: at the margin, from the top of its paragraph.
                    let size = cols.iter().find(|col| col.at == i + k).map_or(sizes.of(i + k, &style), |col| col.size);
                    let x = match side {
                        Wrap::Left => origin.x,
                        Wrap::Right => right - size.x,
                    };
                    Rect::from_min_size(egui::pos2(x, at(i).min.y), size)
                }
                None => {
                    // In the line: the room its character takes, at the bottom of the row.
                    let lead = f32::from(p.indent) * INDENT + if p.list == List::None { 0.0 } else { INDENT };
                    let size = fit(sizes.of(i + k, &style), inline_max(wrap, lead));
                    let room = Rect::from_min_max(egui::pos2(a.min.x + 2.0, a.min.y), egui::pos2(b.min.x.max(a.min.x + 8.0) - 2.0, a.max.y));
                    Rect::from_min_size(egui::pos2(room.min.x, (room.max.y - size.y - 2.0).max(room.min.y)), size)
                }
            };
            ed.placed.push((i + k, shown));
            match tex {
                Some(tex) => {
                    painter.image(tex, shown, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => {
                    painter.rect_stroke(shown, 4.0, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                    icons::paint(&painter, icons::centered(shown, 22.0), Icon::Image, t.dim);
                }
            }
            // Selected: a frame and handles at the corners; dragging one resizes it (as in Word),
            // keeping its proportions.
            let selected = (ed.sel == (i + k..i + k + 1) && (focused || ed.menu_open)) || ed.menu_picture == Some(i + k);
            if selected || ed.resizing.is_some_and(|(r, _)| r == i + k) {
                painter.rect_stroke(shown, 0.0, Stroke::new(1.5, t.accent), egui::StrokeKind::Outside);
                let natural = sizes.of(i + k, &Style { width: None, height: None, ..style.clone() });
                let corners = [
                    (shown.left_top(), egui::CursorIcon::ResizeNwSe, vec2(-1.0, -1.0)),
                    (shown.right_top(), egui::CursorIcon::ResizeNeSw, vec2(1.0, -1.0)),
                    (shown.left_bottom(), egui::CursorIcon::ResizeNeSw, vec2(-1.0, 1.0)),
                    (shown.right_bottom(), egui::CursorIcon::ResizeNwSe, vec2(1.0, 1.0)),
                ];
                for (n_corner, (corner, icon, dir)) in corners.into_iter().enumerate() {
                    let hr = Rect::from_center_size(corner, Vec2::splat(HANDLE));
                    let resp = ui.interact(hr.expand(3.0), egui::Id::unique(("picture-handle", c.id, i + k, n_corner)), Sense::drag());
                    if resp.hovered() || resp.dragged() {
                        ui.ctx().set_cursor_icon(icon);
                    }
                    if resp.drag_started() {
                        ed.resizing = Some((i + k, shown.size()));
                    }
                    if resp.dragged()
                        && let Some((_, size)) = ed.resizing.as_mut()
                    {
                        // The corner follows the pointer along the picture's diagonal.
                        let d = resp.drag_delta();
                        let along = (d.x * dir.x).max(d.y * dir.y * natural.x / natural.y.max(1.0));
                        let w = (size.x + along).clamp(f32::from(rich::PICTURE_MIN_PX), f32::from(rich::PICTURE_MAX_PX));
                        *size = vec2(w, (w * natural.y / natural.x.max(1.0)).max(1.0));
                        ui.ctx().request_repaint();
                    }
                    if resp.drag_stopped()
                        && let Some((r, size)) = ed.resizing.take()
                    {
                        resized = Some((r, size));
                    }
                    painter.rect(hr, 1.0, t.surface, Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
                }
            }
        }
        i += line.chars().count() + 1;
    }
    if let Some((r, size)) = resized {
        ed.sel = r..r + 1;
        let w = size.x.round().clamp(f32::from(rich::PICTURE_MIN_PX), f32::from(rich::PICTURE_MAX_PX));
        let h = size.y.round().clamp(f32::from(rich::PICTURE_MIN_PX), f32::from(rich::PICTURE_MAX_PX));
        // Float to u16 with the range clamped above.
        let (w, h) = (u16::try_from(w as u32).unwrap_or(rich::PICTURE_MAX_PX), u16::try_from(h as u32).unwrap_or(rich::PICTURE_MAX_PX));
        apply(ed, c, Op::Resize(Some([w, h])));
        ed.refocus = Some(r..r + 1);
    }
}

/// A picture's own size in pixels, from its header (never decoded here).
fn natural_size(data: &[u8]) -> Option<Vec2> {
    if data.len() > 30 << 20 {
        return None;
    }
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format().ok()?.into_dimensions().ok()?;
    (w > 0 && h > 0 && w <= 12_000 && h <= 12_000).then(|| vec2(w as f32, h as f32))
}

/// A ribbon button: an icon or a letter, lit when its format is on.
fn fmt_button(ui: &mut Ui, t: &Tokens, face: Face<'_>, on: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    if on || resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, if on { t.accent_soft } else { t.hover });
    }
    match face {
        Face::Icon(ic) => icons::paint(ui.painter(), icons::centered(rect, 16.0), ic, t.text),
        Face::Letter(s, f) => {
            let galley = ui.painter().layout_job(LayoutJob::single_section(s.to_string(), f));
            ui.painter().galley(rect.center() - galley.size() / 2.0, galley, t.text);
        }
    }
    resp.on_hover_text(tl!(tip))
}

enum Face<'a> {
    Icon(Icon),
    Letter(&'a str, TextFormat),
}

/// A colour button with its dropdown (font colour, highlight).
fn color_button(
    ui: &mut Ui,
    t: &Tokens,
    letter: &str,
    current: Option<[u8; 3]>,
    swatches: &[[u8; 3]],
    none: &str,
    tip: &str,
) -> Option<Option<[u8; 3]>> {
    let (rect, resp) = ui.allocate_exact_size(vec2(34.0, 28.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, t.hover);
    }
    ui.painter().text(rect.center() - vec2(4.0, 3.0), egui::Align2::CENTER_CENTER, letter, theme::regular(14.0), t.text);
    let bar = Rect::from_min_size(egui::pos2(rect.min.x + 6.0, rect.max.y - 7.0), vec2(16.0, 3.5));
    let shown = current.map_or(if letter == "A" { t.text } else { Color32::TRANSPARENT }, |c| Color32::from_rgb(c[0], c[1], c[2]));
    ui.painter().rect_filled(bar, 1.0, shown);
    icons::paint(ui.painter(), Rect::from_min_size(egui::pos2(rect.max.x - 11.0, rect.center().y - 4.0), vec2(8.0, 8.0)), Icon::ChevronDown, t.dim);
    let resp = resp.on_hover_text(tl!(tip));
    let mut picked = None;
    egui::Popup::menu(&resp).show(|ui| {
        ui.horizontal_wrapped(|ui| {
            ui.set_max_width(6.0 * 26.0);
            for c in swatches {
                let (r, s) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
                // A stronger edge under the pointer.
                let edge = if s.hovered() { Stroke::new(2.0, t.text) } else { Stroke::new(1.0, t.border) };
                ui.painter().rect(r, 3.0, Color32::from_rgb(c[0], c[1], c[2]), edge, egui::StrokeKind::Inside);
                if s.clicked() {
                    picked = Some(Some(*c));
                }
            }
        });
        if ui.button(tl!(none)).clicked() {
            picked = Some(None);
        }
    });
    picked
}

/// The Message ribbon: the [`format_bar`]'s formatting, links and pictures; then names, files
/// and the signature (`signature` is the sending account's, for Insert › Signature).
pub(crate) fn ribbon(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose, signature: &str, acts: &mut Vec<Action>) {
    let mut ops: Vec<Op> = Vec::new();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        format_items(ui, t, ed, c, acts, &mut ops);
        ui.separator();
        if crate::widgets::tool_button(ui, Icon::AddressBook, "Address Book", t, !c.sending).on_hover_text(tl!("Ctrl+Shift+B")).clicked() {
            acts.push(Action::Dialog(Dialog::select_names(c, "to")));
        }
        if crate::widgets::tool_button(ui, Icon::Attach, "Attach File", t, !c.sending).clicked() {
            acts.push(Action::AttachFiles);
        }
        // The account's signature at the cursor, or making and changing it, from the message
        // (Outlook's Signature menu on the Message ribbon).
        let r = crate::widgets::tool_button(ui, Icon::Drafts, "Signature", t, !c.sending);
        egui::Popup::menu(&r).show(|ui| {
            if ui.add_enabled(!signature.trim().is_empty(), egui::Button::new(tl!("Insert signature"))).clicked() {
                acts.push(Action::InsertSignature);
                ui.close();
            }
            if ui.button(tl!("Signatures…")).clicked() {
                acts.push(Action::EditSignature { account: c.account.clone(), insert: true });
                ui.close();
            }
        });
    });
    link_dialog(ui, t, ed, c);
    apply_ops(ed, c, ops);
}

/// The formatting half of the ribbon on its own (the signature editor's): font size, bold /
/// italic / underline / strikethrough, colours, lists, indent, alignment, clear formatting,
/// links and pictures. The link dialog is the caller's to show.
pub(crate) fn format_bar(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose, acts: &mut Vec<Action>) {
    let mut ops: Vec<Op> = Vec::new();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        format_items(ui, t, ed, c, acts, &mut ops);
    });
    apply_ops(ed, c, ops);
}

fn apply_ops(ed: &mut Editor, c: &mut Compose, ops: Vec<Op>) {
    let changed = !ops.is_empty();
    for op in ops {
        apply(ed, c, op);
    }
    if changed {
        ed.refocus = Some(ed.sel.clone());
    }
}

/// The formatting buttons, then Link and Pictures, in the row being laid out.
fn format_items(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose, acts: &mut Vec<Action>, ops: &mut Vec<Op>) {
    c.styles.fit(&c.body);
    let n = c.body.chars().count();
    let sel = ed.sel.start.min(n)..ed.sel.end.min(n);
    let cur =
        ed.typing.clone().unwrap_or_else(|| if sel.is_empty() { typing_style(&c.body, &c.styles, sel.start) } else { c.styles.style(sel.start) });
    let para = c.styles.para(rich::lines_of(&c.body, sel.start..sel.start).0);
    {
        ui.add_enabled_ui(!c.plain, |ui| {
            egui::ComboBox::from_id_salt(("font-size", c.id)).width(48.0).selected_text(cur.size().to_string()).show_ui(ui, |ui| {
                for z in rich::SIZES {
                    if ui.selectable_label(cur.size() == z, z.to_string()).clicked() {
                        ops.push(Op::Size(z));
                    }
                }
            });
            ui.add_space(4.0);
            let letter = |bold: bool, italic: bool, under: bool, strike: bool| {
                let line = Stroke::new(1.2, t.text);
                TextFormat {
                    font_id: if bold { theme::bold(15.0) } else { theme::regular(15.0) },
                    color: t.text,
                    italics: italic,
                    underline: if under { line } else { Stroke::NONE },
                    strikethrough: if strike { line } else { Stroke::NONE },
                    ..Default::default()
                }
            };
            for (s, face, on, tip, op) in [
                ("B", letter(true, false, false, false), cur.bold, "Bold (Ctrl+B)", Op::Bold),
                ("I", letter(false, true, false, false), cur.italic, "Italic (Ctrl+I)", Op::Italic),
                ("U", letter(false, false, true, false), cur.underline, "Underline (Ctrl+U)", Op::Underline),
                ("ab", letter(false, false, false, true), cur.strike, "Strikethrough", Op::Strike),
            ] {
                if fmt_button(ui, t, Face::Letter(s, face), on, tip).clicked() {
                    ops.push(op);
                }
            }
            if let Some(h) = color_button(ui, t, "ab", cur.highlight, &HIGHLIGHTS, "No color", "Text highlight color") {
                ops.push(Op::Highlight(h));
            }
            if let Some(col) = color_button(ui, t, "A", cur.color, &COLORS, "Automatic", "Font color") {
                ops.push(Op::Color(col));
            }
            ui.separator();
            for (ic, on, tip, op) in [
                (Icon::ListBullet, para.list == List::Bullet, "Bullets (Ctrl+Shift+L)", Op::Bullets),
                (Icon::ListNumber, para.list == List::Number, "Numbering", Op::Numbers),
                (Icon::IndentLess, false, "Decrease indent (Ctrl+Shift+T)", Op::Outdent),
                (Icon::IndentMore, false, "Increase indent (Ctrl+T)", Op::Indent),
                (Icon::AlignLeft, para.align == Align::Left, "Align left (Ctrl+L)", Op::Align(Align::Left)),
                (Icon::AlignCenter, para.align == Align::Center, "Center (Ctrl+E)", Op::Align(Align::Center)),
                (Icon::AlignRight, para.align == Align::Right, "Align right (Ctrl+R)", Op::Align(Align::Right)),
                (Icon::ClearFormat, false, "Clear all formatting (Ctrl+Space)", Op::Clear),
            ] {
                if fmt_button(ui, t, Face::Icon(ic), on, tip).clicked() {
                    ops.push(op);
                }
            }
        });
        ui.separator();
        if crate::widgets::tool_button(ui, Icon::Link, "Link", t, !c.sending && !c.plain).on_hover_text(tl!("Ctrl+K")).clicked() {
            ed.link = Some(LinkForm::open(c, sel.clone()));
        }
        if crate::widgets::tool_button(ui, Icon::Image, "Pictures", t, !c.sending && !c.plain).clicked() {
            acts.push(Action::InsertPicture);
        }
    }
}

/// Insert link, as Outlook's Insert Hyperlink keeps what mail needs: what the text says, and a
/// web page (or file) or an email address with a subject. Shown over the message's own window.
pub(crate) fn link_dialog(ui: &mut Ui, t: &Tokens, ed: &mut Editor, c: &mut Compose) {
    let Some(mut f) = ed.link.take() else { return };
    let mut close = false;
    let mut done: Option<Option<String>> = None;
    let modal = egui::Modal::new(egui::Id::unique(("link-dialog", c.id))).frame(crate::dialogs::dialog_frame(t)).show(ui.ctx(), |ui| {
        ui.set_width(440.0);
        ui.label(RichText::new(tl!(if f.editing { "Edit link" } else { "Insert link" })).font(theme::bold(18.0)).color(t.text));
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(tl!("Link to")).color(t.dim));
            ui.selectable_value(&mut f.email, false, tl!("Web page or file"));
            ui.selectable_value(&mut f.email, true, tl!("Email address"));
        });
        ui.add_space(8.0);
        let first = egui::Grid::new(("link-grid", c.id)).num_columns(2).spacing(vec2(12.0, 8.0)).show(ui, |ui| {
            let w = 300.0;
            ui.label(RichText::new(tl!("Text to display")).color(t.dim));
            if f.multi {
                ui.label(RichText::new(tl!("The selected text")).color(t.faint).italics());
            } else {
                ui.add(egui::TextEdit::singleline(&mut f.text).hint_text(tl!("The address itself")).desired_width(w));
            }
            ui.end_row();
            if f.email {
                ui.label(RichText::new(tl!("Email address")).color(t.dim));
                let r = ui.add(egui::TextEdit::singleline(&mut f.to).hint_text("name@example.com").desired_width(w));
                ui.end_row();
                ui.label(RichText::new(tl!("Subject")).color(t.dim));
                ui.add(egui::TextEdit::singleline(&mut f.subject).desired_width(w));
                ui.end_row();
                r
            } else {
                ui.label(RichText::new(tl!("Address")).color(t.dim));
                let r = ui.add(egui::TextEdit::singleline(&mut f.address).hint_text("https://").desired_width(w));
                ui.end_row();
                r
            }
        });
        if ui.memory(|m| m.focused().is_none()) {
            first.inner.request_focus();
        }
        ui.add_space(12.0);
        let url = f.url();
        let enter = ui.input(|i| i.key_pressed(Key::Enter));
        ui.horizontal(|ui| {
            if f.editing && ui.button(tl!("Remove link")).clicked() {
                done = Some(None);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(url.is_some(), egui::Button::new(tl!("OK"))).clicked() || (enter && url.is_some()) {
                    done = Some(url.clone());
                }
                if ui.button(tl!("Cancel")).clicked() {
                    close = true;
                }
            });
        });
    });
    if modal.should_close() {
        close = true;
    }
    match done {
        Some(Some(url)) => {
            let sel = f.sel.clone();
            let n = c.body.chars().count();
            let sel = sel.start.min(n)..sel.end.min(n);
            let current: String = c.body.chars().skip(sel.start).take(sel.end - sel.start).collect();
            let shown = if f.text.trim().is_empty() {
                url.strip_prefix("mailto:").map_or(url.as_str(), |a| a.split('?').next().unwrap_or(a)).to_string()
            } else {
                f.text.clone()
            };
            if !sel.is_empty() && (shown == current || f.multi) {
                ed.sel = sel;
                apply(ed, c, Op::Link(url));
            } else {
                // New text (or the text changed): typed in place of the selection, as a link.
                let style = Style { link: Some(url), ..typing_style(&c.body, &c.styles, sel.start) };
                replace(ed, c, sel, &shown, style);
                c.plain = false;
            }
            ed.refocus = Some(ed.sel.clone());
        }
        Some(None) => {
            ed.sel = f.sel.clone();
            apply(ed, c, Op::Unlink);
            ed.refocus = Some(ed.sel.clone());
        }
        None if close => ed.refocus = Some(f.sel.clone()),
        None => ed.link = Some(f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture can be set beside the text (Word's wrap text): the pictures the selection means,
    /// and the setting applied to them.
    #[test]
    fn a_picture_goes_beside_the_text() {
        let text = format!("{PICTURE}\nRiley\nx{PICTURE}y");
        let mut c = Compose { body: text.clone(), ..Default::default() };
        c.styles = Styles::plain(&text);
        c.styles.restyle(&text, 0..1, |s| Style { picture: Some("a".into()), ..s.clone() });
        c.styles.restyle(&text, 9..10, |s| Style { picture: Some("b".into()), ..s.clone() });
        assert_eq!(pictures_at(&text, &c.styles, 0..0), vec![0], "the cursor before it");
        assert_eq!(pictures_at(&text, &c.styles, 1..1), vec![0], "the cursor after it");
        assert_eq!(pictures_at(&text, &c.styles, 3..3), Vec::<usize>::new());
        assert_eq!(pictures_at(&text, &c.styles, 0..11), vec![0, 9], "both in the selection");
        let mut ed = Editor::new(1);
        ed.sel = 0..1;
        apply(&mut ed, &mut c, Op::Wrap(Some(Wrap::Left)));
        assert_eq!(c.styles.style(0).wrap, Some(Wrap::Left));
        assert_eq!(c.styles.style(9).wrap, None, "the other is left alone");
        assert_eq!(c.styles.style(0).picture.as_deref(), Some("a"), "still the picture");
        ed.sel = 10..10;
        apply(&mut ed, &mut c, Op::Wrap(Some(Wrap::Right)));
        assert_eq!(c.styles.style(9).wrap, Some(Wrap::Right));
        apply(&mut ed, &mut c, Op::Wrap(None));
        assert_eq!(c.styles.style(9).wrap, None);
        assert!(ed.undo.len() >= 3, "each is a step to undo");
        // Typing after a placed picture isn't placed.
        assert!(typing_style(&text, &c.styles, 1).wrap.is_none());
        // Its size: set (clamped), and back to its own; Clear formatting leaves both alone.
        ed.sel = 0..1;
        apply(&mut ed, &mut c, Op::Resize(Some([120, 60000])));
        assert_eq!((c.styles.style(0).width, c.styles.style(0).height), (Some(120), Some(rich::PICTURE_MAX_PX)));
        apply(&mut ed, &mut c, Op::Clear);
        assert_eq!((c.styles.style(0).width, c.styles.style(0).wrap), (Some(120), Some(Wrap::Left)));
        apply(&mut ed, &mut c, Op::Resize(None));
        assert_eq!(c.styles.style(0).width, None);
        assert!(typing_style(&text, &c.styles, 1).width.is_none());
    }

    #[test]
    fn link_addresses() {
        let web = |a: &str| LinkForm { address: a.into(), ..Default::default() }.url();
        assert_eq!(web("example.com").as_deref(), Some("https://example.com"));
        assert_eq!(web("http://x.example/a").as_deref(), Some("http://x.example/a"));
        assert_eq!(web("C:/Users/me/plan.docx").as_deref(), Some("file:///C:/Users/me/plan.docx"));
        assert_eq!(web("https://"), None);
        assert_eq!(web("  "), None);
        let mail = LinkForm { email: true, to: "zoe@studio.example".into(), subject: "Poster & sizes".into(), ..Default::default() };
        assert_eq!(mail.url().as_deref(), Some("mailto:zoe@studio.example?subject=Poster%20%26%20sizes"));
        assert_eq!(LinkForm { email: true, ..Default::default() }.url(), None);
        // An existing mailto link opens as an email address with its subject.
        let mut c = Compose { body: "write".into(), ..Default::default() };
        c.styles = Styles::plain(&c.body);
        c.styles.restyle(&c.body, 0..5, |s| Style { link: Some("mailto:ann@x.example?subject=Hi%20there".into()), ..s.clone() });
        let f = LinkForm::open(&c, 0..5);
        assert!(f.email && f.editing && !f.multi);
        assert_eq!((f.to.as_str(), f.subject.as_str(), f.text.as_str()), ("ann@x.example", "Hi there", "write"));
    }

    fn laid(ctx: &egui::Context, text: &str, st: &Styles, sizes: &Sizes, wrap: f32) -> Vec<(String, egui::Pos2, f32)> {
        let mut out = Vec::new();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(800.0, 600.0))), ..Default::default() };
        let mut o = ctx.run_ui(input, |ui| {
            let g = layout(ui, text, st, false, &theme::Tokens::light(), wrap, sizes);
            for r in &g.rows {
                out.push((r.row.glyphs.iter().map(|g| g.chr).collect(), r.pos, r.row.size.y));
            }
            assert!(g.rect.min.x <= 0.0 && g.rect.min.y <= 0.0, "the galley's rect starts at its origin: {:?}", g.rect);
        });
        o.textures_delta.clear();
        out
    }

    /// A logo set left of the text: the lines after it, up to a blank one, start right of it;
    /// the lines after that blank go below the logo. In the line, a picture is as tall as set.
    #[test]
    fn a_column_beside_a_logo_and_a_sized_picture_in_the_line() {
        let ctx = egui::Context::default();
        let text = format!("{PICTURE}\nRiley Moreno\nStudio Lantern\n\nBest wishes");
        let mut st = Styles::plain(&text);
        st.restyle(&text, 0..1, |s| Style { picture: Some("logo".into()), wrap: Some(Wrap::Left), ..s.clone() });
        let sizes = Sizes { natural: [("logo".to_string(), vec2(300.0, 200.0))].into_iter().collect(), live: None };
        let rows = laid(&ctx, &text, &st, &sizes, 600.0);
        assert_eq!(rows.len(), 5, "{rows:?}");
        let name = &rows[1];
        assert_eq!(name.0, "Riley Moreno");
        assert!((name.1.x - (300.0 + BESIDE_GAP)).abs() < 0.5, "right of the logo: {rows:?}");
        assert!(name.1.y < 10.0, "from the logo's top: {rows:?}");
        let closing = &rows[4];
        assert_eq!(closing.0, "Best wishes");
        assert!(closing.1.x < 1.0 && closing.1.y >= 200.0, "below the logo: {rows:?}");
        // Set to 120 px wide: the column moves in; the blank line's row sits below the logo.
        st.restyle(&text, 0..1, |s| Style { width: Some(120), ..s.clone() });
        let rows = laid(&ctx, &text, &st, &sizes, 600.0);
        assert!((rows[1].1.x - (120.0 + BESIDE_GAP)).abs() < 0.5, "{rows:?}");
        assert!(rows[4].1.y >= 80.0 && rows[4].1.y < 120.0, "{rows:?}");
        // A wider logo than its share of the width is shown narrower; the drag's size wins.
        st.restyle(&text, 0..1, |s| Style { width: Some(1500), ..s.clone() });
        let rows = laid(&ctx, &text, &st, &sizes, 600.0);
        assert!(rows[1].1.x <= 600.0 * BESIDE_SHARE + BESIDE_GAP + 0.5, "{rows:?}");
        let live = Sizes { live: Some((0, vec2(90.0, 60.0))), ..sizes.clone() };
        let rows = laid(&ctx, &text, &st, &live, 600.0);
        assert!((rows[1].1.x - (90.0 + BESIDE_GAP)).abs() < 0.5, "{rows:?}");
        // In the line: the row is as tall as the picture, as set.
        let text = format!("See {PICTURE} here\nnext");
        let mut st = Styles::plain(&text);
        st.restyle(&text, 4..5, |s| Style { picture: Some("logo".into()), height: Some(150), ..s.clone() });
        let rows = laid(&ctx, &text, &st, &sizes, 600.0);
        assert!(rows[0].2 >= 150.0 && rows[0].2 < 160.0, "{rows:?}");
        assert!(rows[1].1.y >= 150.0, "{rows:?}");
        assert_eq!(sized(&st.style(4), vec2(300.0, 200.0)), vec2(225.0, 150.0));
        assert_eq!(fit(vec2(3000.0, 10.0), vec2(300.0, 300.0)), vec2(300.0, 1.0));
        assert_eq!(sized(&Style::default(), vec2(0.0, 0.0)), vec2(1.0, 1.0));
    }
}
