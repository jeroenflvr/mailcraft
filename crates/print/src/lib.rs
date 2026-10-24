//! A message on paper: laid out on A4 pages and written as a PDF (ISO 32000).
//!
//! The caller hands over font files (the system's, found by the app: a regular and a bold face
//! and fallbacks for the scripts and symbols the text uses). Each character is drawn from the
//! first face that has it, so a line is split into runs by face; a TrueType face goes in as a CID
//! font (`FontFile2`), an OpenType one with CFF outlines as `FontFile3`, a face of a `.ttc`
//! collection is cut out of it, and every embedded face carries a `ToUnicode` map so the text
//! can be searched and copied. A character no face has is drawn as `?`, never as the font's
//! missing-glyph box. Without any font the standard Helvetica is used and characters outside
//! Windows-1252 print as `?`. Pages carry the subject, the header, the text and page numbers.
//! Pure Rust, no I/O: the caller reads the fonts and writes the bytes.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;

/// What goes on the pages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    /// Printed large at the top (the subject).
    pub title: String,
    /// `From`, `To`, `Date`… lines under it.
    pub header: Vec<(String, String)>,
    /// The text.
    pub body: String,
}

impl Doc {
    /// Everything on the pages as one text: what fonts must have characters for.
    pub fn text(&self) -> String {
        let header: Vec<String> = self.header.iter().map(|(k, v)| format!("{k}: {v}")).collect();
        format!("{}\n{}\n{}", self.title, header.join("\n"), self.body)
    }
}

/// A font file: TrueType or OpenType, or a `.ttc` collection with the number of the face to use.
#[derive(Clone, Debug)]
pub struct FontFile {
    pub data: Arc<[u8]>,
    pub index: u32,
}

impl FontFile {
    pub fn new(data: impl Into<Arc<[u8]>>, index: u32) -> FontFile {
        FontFile { data: data.into(), index }
    }
}

/// The fonts to print with, in the order they are tried for a character: the regular face (the
/// bold one for bold text, then the regular), then the fallbacks.
#[derive(Clone, Debug, Default)]
pub struct Fonts {
    pub regular: Option<FontFile>,
    pub bold: Option<FontFile>,
    pub fallbacks: Vec<FontFile>,
}

/// What a document needed that it didn't get.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Characters no font had (drawn as `?`), each once, the first 50.
    pub missing: Vec<char>,
    /// Fallback fonts left out because the embedded fonts would have passed [`MAX_EMBEDDED`].
    pub dropped_fonts: usize,
}

/// The most font data one document embeds; the largest fallbacks beyond it are left out.
pub const MAX_EMBEDDED: usize = 40 << 20;

const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN: f32 = 56.0;
/// Most pages written (a very long message is cut, with a note).
const MAX_PAGES: usize = 500;
/// Most fallback fonts looked at.
const MAX_FALLBACKS: usize = 16;

/// Whether a character is drawn at all: zero-width and format characters (zero-width space and
/// joiners, direction marks, the word joiner, variation selectors, the byte order mark, the soft
/// hyphen) and control characters are dropped.
fn printable(c: char) -> bool {
    !(matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{00AD}'
            | '\u{E0100}'..='\u{E01EF}'
    ) || (c.is_control() && !matches!(c, '\n' | '\t')))
}

/// The text as it is drawn: line and paragraph separators are line breaks, the characters
/// [`printable`] says no to are gone.
fn clean(s: &str) -> String {
    s.replace("\r\n", "\n").chars().map(|c| if matches!(c, '\u{2028}' | '\u{2029}' | '\r') { '\n' } else { c }).filter(|c| printable(*c)).collect()
}

/// One face of a font file as a plain `sfnt` (a `.ttc`'s face is cut out of the collection: a PDF
/// embeds one font program).
fn sfnt(file: &FontFile) -> Option<Cow<'_, [u8]>> {
    let data: &[u8] = &file.data;
    if data.get(..4)? != b"ttcf" {
        return (file.index == 0).then_some(Cow::Borrowed(data));
    }
    let be32 = |at: usize| data.get(at..at + 4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_be_bytes);
    let be16 = |at: usize| data.get(at..at + 2).and_then(|b| <[u8; 2]>::try_from(b).ok()).map(u16::from_be_bytes);
    if file.index >= be32(8)? {
        return None;
    }
    let head = usize::try_from(be32(12 + 4 * usize::try_from(file.index).ok()?)?).ok()?;
    let count = usize::from(be16(head.checked_add(4)?)?);
    if count == 0 || count > 128 {
        return None;
    }
    let mut out = data.get(head..head + 12)?.to_vec();
    let mut tables: Vec<&[u8]> = Vec::with_capacity(count);
    for i in 0..count {
        let rec = head.checked_add(12 + 16 * i)?;
        let (off, len) = (usize::try_from(be32(rec + 8)?).ok()?, usize::try_from(be32(rec + 12)?).ok()?);
        tables.push(data.get(off..off.checked_add(len)?)?);
        out.extend_from_slice(data.get(rec..rec + 8)?);
        out.extend_from_slice(&[0; 8]);
    }
    // Table data after the directory, each on a 4-byte boundary; the directory learns where.
    for (i, t) in tables.iter().enumerate() {
        while out.len() % 4 != 0 {
            out.push(0);
        }
        let at = u32::try_from(out.len()).ok()?;
        let dir = 12 + 16 * i + 8;
        out.get_mut(dir..dir + 4)?.copy_from_slice(&at.to_be_bytes());
        out.get_mut(dir + 4..dir + 8)?.copy_from_slice(&u32::try_from(t.len()).ok()?.to_be_bytes());
        out.extend_from_slice(t);
    }
    Some(Cow::Owned(out))
}

/// A font as the layout and the writer see it.
enum Face<'a> {
    Embedded { bytes: &'a [u8], face: Box<ttf_parser::Face<'a>>, cff: bool, used: BTreeMap<u16, char>, enabled: bool },
    Helvetica { bold: bool, used: bool },
}

impl<'a> Face<'a> {
    /// A plain `sfnt` with outlines a PDF can embed (TrueType or CFF).
    fn parse(bytes: &'a [u8]) -> Option<Face<'a>> {
        let face = ttf_parser::Face::parse(bytes, 0).ok()?;
        let tables = face.tables();
        let cff = tables.cff.is_some();
        if tables.glyf.is_none() && !cff {
            return None;
        }
        Some(Face::Embedded { bytes, face: Box::new(face), cff, used: BTreeMap::new(), enabled: true })
    }

    fn glyph(&self, c: char) -> Option<u16> {
        match self {
            Face::Embedded { face, .. } => face.glyph_index(c).map(|g| g.0).filter(|g| *g != 0),
            Face::Helvetica { .. } => None,
        }
    }

    /// Whether the face draws `c` (never as a missing-glyph box).
    fn covers(&self, c: char) -> bool {
        match self {
            Face::Embedded { enabled, .. } => *enabled && self.glyph(c).is_some(),
            Face::Helvetica { .. } => c == '?' || win_ansi(c) != b'?',
        }
    }

    /// The font program's size in bytes (what embedding it costs).
    fn size(&self) -> usize {
        match self {
            Face::Embedded { bytes, .. } => bytes.len(),
            Face::Helvetica { .. } => 0,
        }
    }

    fn is_used(&self) -> bool {
        match self {
            Face::Embedded { used, .. } => !used.is_empty(),
            Face::Helvetica { used, .. } => *used,
        }
    }

    /// Width of a character in thousandths of the font size.
    fn width(&self, c: char) -> f32 {
        match self {
            Face::Embedded { face, .. } => {
                let upem = f32::from(face.units_per_em().max(1));
                let adv = self.glyph(c).map_or(0, |g| face.glyph_hor_advance(ttf_parser::GlyphId(g)).unwrap_or(0));
                f32::from(adv) * 1000.0 / upem
            }
            Face::Helvetica { bold, .. } => helvetica_width(c, *bold),
        }
    }

    /// The bytes of a PDF string drawing `s`, whose characters the face covers.
    fn encode(&mut self, s: &str) -> String {
        match self {
            Face::Embedded { face, used, .. } => {
                let mut hex = String::with_capacity(s.len() * 4 + 2);
                hex.push('<');
                for c in s.chars() {
                    let Some(gid) = face.glyph_index(c).map(|g| g.0).filter(|g| *g != 0) else { continue };
                    used.entry(gid).or_insert(c);
                    let _ = write!(hex, "{gid:04X}");
                }
                hex.push('>');
                hex
            }
            Face::Helvetica { used, .. } => {
                *used = true;
                let mut lit = String::from("(");
                for c in s.chars() {
                    match win_ansi(c) {
                        b'(' | b')' | b'\\' => {
                            lit.push('\\');
                            lit.push(char::from(win_ansi(c)));
                        }
                        b if b >= 0x80 => {
                            let _ = write!(lit, "\\{b:03o}");
                        }
                        b => lit.push(char::from(b)),
                    }
                }
                lit.push(')');
                lit
            }
        }
    }
}

/// Windows-1252 code of a character (`?` when it has none).
fn win_ansi(c: char) -> u8 {
    match c {
        ' '..='~' => c as u8,
        '\u{A0}'..='\u{FF}' => u8::try_from(u32::from(c)).unwrap_or(b'?'),
        '€' => 0x80,
        '‚' => 0x82,
        '„' => 0x84,
        '…' => 0x85,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '™' => 0x99,
        _ => b'?',
    }
}

/// Helvetica's widths (the standard metrics of the PDF base fonts) for printable ASCII; other
/// characters get the width of `n`.
fn helvetica_width(c: char, bold: bool) -> f32 {
    const REGULAR: [u16; 95] = [
        278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278,
        584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944,
        667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500,
        278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,
    ];
    const BOLD: [u16; 95] = [
        278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 333, 333,
        584, 584, 584, 611, 975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833, 722, 778, 667, 778, 722, 667, 611, 722, 667, 944,
        667, 667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611, 556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556,
        333, 611, 556, 778, 556, 556, 500, 389, 280, 389, 584,
    ];
    let table = if bold { &BOLD } else { &REGULAR };
    let i = (c as usize).wrapping_sub(32);
    f32::from(table.get(i).copied().unwrap_or(556))
}

/// The faces of a document: 0 regular, 1 bold, then the fallbacks, then (after an embedded
/// regular) Helvetica, which draws a `?` for whatever nothing else has.
struct Faces<'a> {
    list: Vec<Face<'a>>,
    /// (style, character) → (face, width in thousandths), so each is looked up once.
    cache: RefCell<HashMap<(usize, char), Option<(usize, f32)>>>,
}

impl<'a> Faces<'a> {
    /// `sources` are the font programs, regular, bold, then the fallbacks (`None`: unusable).
    fn new(sources: &'a [Option<Cow<'a, [u8]>>]) -> Faces<'a> {
        let parse = |i: usize| sources.get(i).and_then(Option::as_deref).and_then(Face::parse);
        let regular = parse(0);
        let embedded = regular.is_some();
        let mut list = vec![regular.unwrap_or(Face::Helvetica { bold: false, used: false })];
        list.push(parse(1).unwrap_or(Face::Helvetica { bold: true, used: false }));
        list.extend((2..sources.len()).filter_map(parse));
        if embedded {
            list.push(Face::Helvetica { bold: false, used: false });
        }
        Faces { list, cache: RefCell::new(HashMap::new()) }
    }

    /// The face that draws `c` in a style (0 regular, 1 bold): the first of the style's own face
    /// (bold, then the regular one), the regular one and the fallbacks that has it.
    fn pick_uncached(&self, style: usize, c: char) -> Option<usize> {
        let first: &[usize] = if style == 1 { &[1, 0] } else { &[0] };
        first.iter().copied().chain(2..self.list.len()).find(|i| self.list.get(*i).is_some_and(|f| f.covers(c)))
    }

    /// (face, width) for `c`; a character nothing has is drawn as `?`.
    fn locate(&self, style: usize, c: char) -> (usize, char, f32) {
        let hit = *self.cache.borrow_mut().entry((style, c)).or_insert_with(|| {
            let i = self.pick_uncached(style, c)?;
            Some((i, self.list.get(i)?.width(c)))
        });
        match hit {
            Some((i, w)) => (i, c, w),
            None => match self.pick_uncached(style, '?').and_then(|i| Some((i, self.list.get(i)?.width('?')))) {
                Some((i, w)) => (i, '?', w),
                None => (0, '?', 556.0),
            },
        }
    }

    fn text_width(&self, style: usize, s: &str, size: f32) -> f32 {
        s.chars().map(|c| self.locate(style, c).2).sum::<f32>() * size / 1000.0
    }

    /// `s` as runs, each in one face: (face, text drawn, width in thousandths).
    fn runs(&self, style: usize, s: &str) -> Vec<(usize, String, f32)> {
        let mut out: Vec<(usize, String, f32)> = Vec::new();
        for c in s.chars() {
            let (i, c, w) = self.locate(style, c);
            match out.last_mut() {
                Some((j, text, total)) if *j == i => {
                    text.push(c);
                    *total += w;
                }
                _ => out.push((i, c.to_string(), w)),
            }
        }
        out
    }

    /// Leave out the largest fallbacks while the faces `text` needs would pass `cap` bytes; how many.
    fn enforce_cap(&mut self, text: &str, cap: usize) -> usize {
        let chars: BTreeSet<char> = text.chars().collect();
        let mut dropped = 0;
        loop {
            let used: BTreeSet<usize> = chars.iter().flat_map(|c| [self.pick_uncached(0, *c), self.pick_uncached(1, *c)]).flatten().collect();
            if used.iter().filter_map(|i| self.list.get(*i)).map(Face::size).sum::<usize>() <= cap {
                return dropped;
            }
            let biggest = used.iter().copied().filter(|i| *i >= 2).max_by_key(|i| self.list.get(*i).map_or(0, Face::size));
            let Some(Face::Embedded { enabled, .. }) = biggest.and_then(|i| self.list.get_mut(i)) else { return dropped };
            *enabled = false;
            dropped += 1;
            self.cache.borrow_mut().clear();
        }
    }
}

/// A line of text to place: style (0 regular, 1 bold), size, position.
struct Placed {
    style: usize,
    size: f32,
    x: f32,
    y: f32,
    text: String,
}

/// Split `text` into lines no wider than `width` (words, then characters for long words).
fn wrap(faces: &Faces<'_>, style: usize, text: &str, size: f32, width: f32) -> Vec<String> {
    let wide = |s: &str| faces.text_width(style, s, size);
    let mut out = Vec::new();
    for para in text.replace("\r\n", "\n").split('\n') {
        let para = para.replace('\t', "    ");
        let mut line = String::new();
        // Keep leading spaces (quoting, indentation).
        let lead: String = para.chars().take_while(|c| *c == ' ').collect();
        let words: Vec<&str> = para.split(' ').filter(|w| !w.is_empty()).collect();
        if words.is_empty() {
            out.push(String::new());
            continue;
        }
        line.push_str(&lead);
        for w in words {
            let candidate = if line.trim().is_empty() { format!("{line}{w}") } else { format!("{line} {w}") };
            if wide(&candidate) <= width {
                line = candidate;
                continue;
            }
            if !line.trim().is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // A word wider than the line is cut by characters.
            let mut piece = String::new();
            for c in w.chars() {
                piece.push(c);
                if wide(&piece) > width && piece.chars().count() > 1 {
                    let last = piece.pop().unwrap_or(' ');
                    out.push(std::mem::take(&mut piece));
                    piece.push(last);
                }
            }
            line = piece;
        }
        out.push(line);
    }
    out
}

fn compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

/// PDF text-string literal of `s` (for the document info), escaped; non-ASCII as `?`.
fn info_string(s: &str) -> String {
    let mut out = String::from("(");
    for c in s.chars().take(200) {
        match c {
            '(' | ')' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            ' '..='~' => out.push(c),
            _ => out.push('?'),
        }
    }
    out.push(')');
    out
}

/// Draw `text` at `at` as one `Tj` per run of one face.
fn show(c: &mut String, faces: &mut Faces<'_>, style: usize, size: f32, at: (f32, f32), text: &str) {
    let mut x = at.0;
    for (face, run, w) in faces.runs(style, text) {
        let enc = faces.list.get_mut(face).map(|f| f.encode(&run)).unwrap_or_default();
        let _ = writeln!(c, "/F{} {size:.1} Tf 1 0 0 1 {x:.2} {:.2} Tm {enc} Tj", face + 1, at.1);
        x += w * size / 1000.0;
    }
}

/// The document as PDF bytes.
pub fn pdf(doc: &Doc, fonts: &Fonts) -> Result<Vec<u8>, String> {
    pdf_report(doc, fonts).map(|(bytes, _)| bytes)
}

/// The document as PDF bytes, and what it asked of the fonts that it didn't get.
pub fn pdf_report(doc: &Doc, fonts: &Fonts) -> Result<(Vec<u8>, Report), String> {
    pdf_capped(doc, fonts, MAX_EMBEDDED)
}

fn pdf_capped(doc: &Doc, fonts: &Fonts, cap: usize) -> Result<(Vec<u8>, Report), String> {
    let title = match clean(&doc.title) {
        t if t.trim().is_empty() => "(no subject)".to_string(),
        t => t,
    };
    let header: Vec<(String, String)> = doc.header.iter().map(|(k, v)| (clean(k), clean(v))).collect();
    let body = clean(&doc.body);

    let sources: Vec<Option<Cow<'_, [u8]>>> = [fonts.regular.as_ref(), fonts.bold.as_ref()]
        .into_iter()
        .chain(fonts.fallbacks.iter().take(MAX_FALLBACKS).map(Some))
        .map(|f| f.and_then(sfnt))
        .collect();
    let mut faces = Faces::new(&sources);
    let mut everything = format!("{title}\n{body}\n0123456789 /");
    for (k, v) in &header {
        everything.push_str(&format!("\n{k}:{v}"));
    }
    let dropped_fonts = faces.enforce_cap(&everything, cap);
    let mut missing: Vec<char> = Vec::new();
    for c in everything.chars().collect::<BTreeSet<_>>() {
        if !matches!(c, '\n' | '\t') && faces.pick_uncached(0, c).is_none() && missing.len() < 50 {
            missing.push(c);
        }
    }
    let width = PAGE_W - 2.0 * MARGIN;

    // Lay out.
    let mut pages: Vec<Vec<Placed>> = vec![Vec::new()];
    let mut y = PAGE_H - MARGIN;
    let mut truncated = false;
    let place = |pages: &mut Vec<Vec<Placed>>, y: &mut f32, style: usize, size: f32, lead: f32, x: f32, text: String| -> bool {
        if *y - lead < MARGIN + 20.0 {
            if pages.len() >= MAX_PAGES {
                return false;
            }
            pages.push(Vec::new());
            *y = PAGE_H - MARGIN;
        }
        *y -= lead;
        if let Some(p) = pages.last_mut() {
            p.push(Placed { style, size, x, y: *y, text });
        }
        true
    };
    for line in wrap(&faces, 1, &title, 16.0, width) {
        place(&mut pages, &mut y, 1, 16.0, 21.0, MARGIN, line);
    }
    y -= 6.0;
    let label_w = header.iter().map(|(k, _)| faces.text_width(1, &format!("{k}:"), 9.5)).fold(0.0f32, f32::max) + 6.0;
    for (k, v) in &header {
        let lines = wrap(&faces, 0, v, 9.5, width - label_w);
        for (i, line) in lines.into_iter().enumerate() {
            if i == 0 {
                let ok = place(&mut pages, &mut y, 1, 9.5, 13.0, MARGIN, format!("{k}:"));
                if ok && let Some(p) = pages.last_mut() {
                    p.push(Placed { style: 0, size: 9.5, x: MARGIN + label_w, y, text: line });
                }
            } else {
                place(&mut pages, &mut y, 0, 9.5, 13.0, MARGIN + label_w, line);
            }
        }
    }
    y -= 8.0;
    let rule_page = pages.len() - 1;
    let rule_y = y;
    y -= 6.0;
    for line in wrap(&faces, 0, &body, 11.0, width) {
        if !place(&mut pages, &mut y, 0, 11.0, 14.5, MARGIN, line) {
            truncated = true;
            break;
        }
    }
    if truncated {
        place(&mut pages, &mut y, 1, 11.0, 18.0, MARGIN, "(The rest of the message didn't fit in this document.)".into());
    }

    // Content streams: each line in runs, one per face.
    let n = pages.len();
    let mut contents: Vec<String> = Vec::with_capacity(n);
    for (i, page) in pages.iter().enumerate() {
        let mut c = String::new();
        if i == rule_page {
            let _ = writeln!(c, "0.75 G 0.6 w {MARGIN:.2} {rule_y:.2} m {:.2} {rule_y:.2} l S 0 G", PAGE_W - MARGIN);
        }
        c.push_str("BT\n");
        for p in page {
            show(&mut c, &mut faces, p.style, p.size, (p.x, p.y), &p.text);
        }
        let footer = format!("{} / {n}", i + 1);
        let fw = faces.text_width(0, &footer, 8.0);
        c.push_str("0.45 g\n");
        show(&mut c, &mut faces, 0, 8.0, ((PAGE_W - fw) / 2.0, MARGIN / 2.0), &footer);
        c.push_str("0 g\n");
        c.push_str("ET\n");
        contents.push(c);
    }

    // Objects: 1 catalog, 2 pages, 3 info, then the fonts used, then page + content pairs.
    let mut objects: Vec<Vec<u8>> = Vec::new();
    let add = |objects: &mut Vec<Vec<u8>>, body: Vec<u8>| -> usize {
        objects.push(body);
        objects.len()
    };
    let stream = |dict: &str, data: &[u8]| -> Vec<u8> {
        let mut s = format!("<< {dict} /Length {} /Filter /FlateDecode >>\nstream\n", data.len()).into_bytes();
        s.extend_from_slice(data);
        s.extend_from_slice(b"\nendstream");
        s
    };
    add(&mut objects, Vec::new()); // catalog, filled in last
    add(&mut objects, Vec::new()); // pages, filled in last
    add(&mut objects, format!("<< /Producer (MailCraft {}) /Title {} >>", env!("CARGO_PKG_VERSION"), info_string(&title)).into_bytes());
    let mut font_refs: Vec<Option<usize>> = Vec::new();
    for (i, f) in faces.list.iter().enumerate() {
        if !f.is_used() {
            font_refs.push(None);
            continue;
        }
        let r = match f {
            Face::Helvetica { bold, .. } => add(
                &mut objects,
                format!(
                    "<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>",
                    if *bold { "Helvetica-Bold" } else { "Helvetica" }
                )
                .into_bytes(),
            ),
            Face::Embedded { bytes, face, cff, used, .. } => {
                let upem = f32::from(face.units_per_em().max(1));
                let scale = |v: i16| (f32::from(v) * 1000.0 / upem).round() as i32;
                // TrueType outlines go in as a `FontFile2`; an OpenType font with CFF outlines
                // whole, as a `FontFile3` (PDF 1.6, ISO 32000-1 §9.9).
                let file_ref = add(
                    &mut objects,
                    if *cff {
                        stream("/Subtype /OpenType", &compress(bytes))
                    } else {
                        stream(&format!("/Length1 {}", bytes.len()), &compress(bytes))
                    },
                );
                let bbox = face.global_bounding_box();
                let descriptor = format!(
                    "<< /Type /FontDescriptor /FontName /MailCraftFont{i} /Flags 32 /FontBBox [{} {} {} {}] /ItalicAngle 0 /Ascent {} /Descent {} /CapHeight {} /StemV {} /{} {file_ref} 0 R >>",
                    scale(bbox.x_min),
                    scale(bbox.y_min),
                    scale(bbox.x_max),
                    scale(bbox.y_max),
                    scale(face.ascender()),
                    scale(face.descender()),
                    scale(face.capital_height().unwrap_or(face.ascender())),
                    if i == 1 { 140 } else { 80 },
                    if *cff { "FontFile3" } else { "FontFile2" }
                );
                let desc_ref = add(&mut objects, descriptor.into_bytes());
                let mut w = String::from("[");
                for gid in used.keys() {
                    let adv = face.glyph_hor_advance(ttf_parser::GlyphId(*gid)).unwrap_or(0);
                    let _ = write!(w, " {gid} [{}]", (f32::from(adv) * 1000.0 / upem).round() as i32);
                }
                w.push_str(" ]");
                let (kind, map) = if *cff { ("CIDFontType0", "") } else { ("CIDFontType2", " /CIDToGIDMap /Identity") };
                let cid = format!(
                    "<< /Type /Font /Subtype /{kind} /BaseFont /MailCraftFont{i} /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {desc_ref} 0 R /DW 1000 /W {w}{map} >>"
                );
                let cid_ref = add(&mut objects, cid.into_bytes());
                let mut cmap = String::from(
                    "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def /CMapName /Adobe-Identity-UCS def /CMapType 2 def 1 begincodespacerange <0000> <FFFF> endcodespacerange\n",
                );
                let pairs: Vec<(&u16, &char)> = used.iter().collect();
                for chunk in pairs.chunks(100) {
                    let _ = writeln!(cmap, "{} beginbfchar", chunk.len());
                    for (gid, c) in chunk {
                        let mut units = [0u16; 2];
                        let hex: String = c.encode_utf16(&mut units).iter().map(|u| format!("{u:04X}")).collect();
                        let _ = writeln!(cmap, "<{gid:04X}> <{hex}>");
                    }
                    cmap.push_str("endbfchar\n");
                }
                cmap.push_str("endcmap CMapName currentdict /CMap defineresource pop end end");
                let tu_ref = add(&mut objects, stream("", &compress(cmap.as_bytes())));
                add(
                    &mut objects,
                    format!("<< /Type /Font /Subtype /Type0 /BaseFont /MailCraftFont{i} /Encoding /Identity-H /DescendantFonts [{cid_ref} 0 R] /ToUnicode {tu_ref} 0 R >>")
                        .into_bytes(),
                )
            }
        };
        font_refs.push(Some(r));
    }
    let fonts_dict: String =
        font_refs.iter().enumerate().filter_map(|(i, r)| r.map(|r| format!("/F{} {r} 0 R", i + 1))).collect::<Vec<_>>().join(" ");
    let mut kids = Vec::new();
    for c in &contents {
        let content_ref = add(&mut objects, stream("", &compress(c.as_bytes())));
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W:.2} {PAGE_H:.2}] /Resources << /Font << {fonts_dict} >> >> /Contents {content_ref} 0 R >>"
        );
        kids.push(add(&mut objects, page.into_bytes()));
    }
    if let Some(o) = objects.get_mut(0) {
        *o = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();
    }
    if let Some(o) = objects.get_mut(1) {
        let k: String = kids.iter().map(|r| format!("{r} 0 R")).collect::<Vec<_>>().join(" ");
        *o = format!("<< /Type /Pages /Kids [{k}] /Count {} >>", kids.len()).into_bytes();
    }

    // The file: header, objects, cross-reference table, trailer.
    let mut out: Vec<u8> = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R /Info 3 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    Ok((out, Report { missing, dropped_fonts }))
}

impl Fonts {
    /// The distinct characters of `text` that nothing here draws (Helvetica, which has Windows-1252,
    /// is always the last resort): what the app should look for a fallback font for.
    pub fn missing(&self, text: &str) -> Vec<char> {
        let files: Vec<&FontFile> = self.regular.iter().chain(&self.fallbacks).collect();
        clean(text)
            .chars()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|c| !c.is_whitespace() && win_ansi(*c) == b'?' && *c != '?' && !files.iter().any(|f| covers(f, *c)))
            .collect()
    }
}

/// Whether a font file has a glyph for `c` and can be embedded (TrueType or CFF outlines). Reads
/// the file where it is, so asking about many characters is cheap.
pub fn covers(file: &FontFile, c: char) -> bool {
    let Ok(face) = ttf_parser::Face::parse(&file.data, file.index) else { return false };
    let tables = face.tables();
    (tables.glyf.is_some() || tables.cff.is_some()) && face.glyph_index(c).is_some_and(|g| g.0 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> Doc {
        Doc {
            title: "Storyboard (v2) review".into(),
            header: vec![("From".into(), "Zoe Park <zoe@studio.example>".into()), ("Date".into(), "Wed 14 Oct 2026 09:30".into())],
            body: body.into(),
        }
    }

    /// The first font file of these that exists.
    fn font(paths: &[&str], index: u32) -> Option<FontFile> {
        paths.iter().find_map(|p| std::fs::read(p).ok()).map(|d| FontFile::new(d, index))
    }

    fn noto() -> Option<FontFile> {
        font(
            &[
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                "C:\\Windows\\Fonts\\segoeui.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/System/Library/Fonts/Supplemental/Arial.ttf",
            ],
            0,
        )
    }

    fn dejavu() -> Option<FontFile> {
        font(&["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/dejavu/DejaVuSans.ttf", "C:\\Windows\\Fonts\\seguisym.ttf"], 0)
    }

    /// A collection of OpenType fonts with CFF outlines (Noto Sans CJK).
    fn cjk_collection() -> Option<FontFile> {
        font(&["/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"], 0)
    }

    /// Every object starts exactly where the cross-reference table says (on the bytes: the
    /// streams are binary).
    fn objects_are_where_the_xref_says(pdf: &[u8]) {
        let find_last = |needle: &[u8]| pdf.windows(needle.len()).rposition(|w| w == needle);
        let sx = find_last(b"startxref\n").unwrap() + 10;
        let tail = String::from_utf8_lossy(&pdf[sx..]);
        let start: usize = tail.lines().next().unwrap().trim().parse().unwrap();
        assert!(pdf[start..].starts_with(b"xref"));
        let table = String::from_utf8_lossy(&pdf[start..sx]).to_string();
        let mut n = 0;
        for (i, line) in table.lines().skip(3).take_while(|l| l.ends_with(" n ")).enumerate() {
            let off: usize = line[..10].parse().unwrap();
            assert!(pdf[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()), "object {} at {off}", i + 1);
            n += 1;
        }
        assert!(n >= 4);
    }

    /// The page content streams, decompressed.
    fn page_texts(pdf: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut at = 0;
        while let Some(i) = pdf[at..].windows(7).position(|w| w == b"stream\n") {
            let from = at + i + 7;
            let Some(len) = pdf[from..].windows(10).position(|w| w == b"\nendstream") else { break };
            if let Ok(raw) = miniz_oxide::inflate::decompress_to_vec_zlib(&pdf[from..from + len])
                && let Ok(text) = String::from_utf8(raw)
                && text.starts_with("0.75 G") | text.starts_with("BT")
            {
                out.push(text);
            }
            at = from + len;
        }
        out
    }

    /// Every glyph id the pages draw with a CID font (the `<…>` strings, four digits each).
    fn glyph_ids(pdf: &[u8]) -> Vec<String> {
        let mut ids = Vec::new();
        for page in page_texts(pdf) {
            for line in page.lines().filter(|l| l.contains(" Tj")) {
                if let Some(hex) = line.split('<').nth(1).and_then(|r| r.split('>').next()) {
                    ids.extend(hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).to_string()));
                }
            }
        }
        ids
    }

    fn count(pdf: &[u8], needle: &str) -> usize {
        String::from_utf8_lossy(pdf).matches(needle).count()
    }

    #[test]
    fn helvetica_pages() {
        let long = (0..400).map(|i| format!("Line {i}: the quick brown fox jumps over the lazy dog (café, “quotes”)")).collect::<Vec<_>>().join("\n");
        let p = pdf(&doc(&long), &Fonts::default()).unwrap();
        assert!(p.starts_with(b"%PDF-1.7"));
        assert!(p.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&p);
        assert!(text.contains("/BaseFont /Helvetica"));
        let count: usize = text.split("/Count ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse().ok()).unwrap();
        assert!(count >= 8, "{count} pages");
        assert!(text.contains("/Title (Storyboard \\(v2\\) review)"));
        objects_are_where_the_xref_says(&p);
    }

    #[test]
    fn wrapping() {
        let sources = [];
        let f = Faces::new(&sources);
        let lines = wrap(&f, 0, "a b c", 11.0, 1000.0);
        assert_eq!(lines, vec!["a b c".to_string()]);
        let lines = wrap(&f, 0, &"word ".repeat(100), 11.0, 200.0);
        assert!(lines.len() > 5 && lines.iter().all(|l| f.text_width(0, l, 11.0) <= 200.0));
        let lines = wrap(&f, 0, &"x".repeat(500), 11.0, 100.0);
        assert!(lines.len() > 5 && lines.iter().all(|l| f.text_width(0, l, 11.0) <= 100.0), "long words are cut");
        assert_eq!(wrap(&f, 0, "> quoted\n\n  indented", 11.0, 400.0), vec!["> quoted".to_string(), String::new(), "  indented".to_string()]);
        assert_eq!(win_ansi('é'), 0xE9);
        assert_eq!(win_ansi('€'), 0x80);
        assert_eq!(win_ansi('漢'), b'?');
    }

    /// Zero-width and format characters, and control characters, are never drawn; separators break lines.
    #[test]
    fn invisible_characters_are_dropped() {
        assert_eq!(clean("a\u{200B}b\u{200D}c\u{FE0F}d\u{FEFF}e\u{00AD}f\u{2060}g\u{200E}h"), "abcdefgh");
        assert_eq!(clean("x\u{2028}y\u{2029}z\r\nw\rv"), "x\ny\nz\nw\nv");
        assert_eq!(clean("a\u{0}b\u{7}c\td\ne"), "abc\td\ne");
        let p = pdf(&doc("A\u{FE0F}B\u{200B}C"), &Fonts::default()).unwrap();
        let pages = page_texts(&p).join("\n");
        assert!(pages.contains("(ABC)") && !pages.contains('?'), "{pages}");
    }

    #[test]
    fn empty_and_hostile_input() {
        let p = pdf(&Doc::default(), &Fonts::default()).unwrap();
        objects_are_where_the_xref_says(&p);
        // Not a font: falls back to Helvetica rather than failing.
        let junk = Fonts {
            regular: Some(FontFile::new(&b"not a font"[..], 0)),
            bold: Some(FontFile::new(&b""[..], 3)),
            fallbacks: vec![FontFile::new(&b"ttcf"[..], 0)],
        };
        let p = pdf(&doc("x"), &junk).unwrap();
        assert!(String::from_utf8_lossy(&p).contains("/Helvetica"));
        let huge = "y\n".repeat(200_000);
        let p = pdf(&doc(&huge), &Fonts::default()).unwrap();
        assert!(String::from_utf8_lossy(&p).contains(&format!("/Count {MAX_PAGES}")));
        // A collection that is cut short, one whose directory points outside it, and a face that
        // isn't there.
        let mut bad: Vec<Vec<u8>> =
            vec![b"ttcf\x00\x01\x00\x00\x00\x00\x00\x02".to_vec(), b"ttcf\x00\x01\x00\x00\x00\x00\x00\x01\x00\x00\x00\x10".to_vec()];
        if let Some(c) = cjk_collection() {
            bad.push(c.data[..c.data.len().min(5000)].to_vec());
            bad.push(c.data[..c.data.len() / 2].to_vec());
        }
        for (i, data) in bad.into_iter().enumerate() {
            let f = Fonts { regular: Some(FontFile::new(data, 0)), fallbacks: vec![FontFile::new(vec![0u8; 64], u32::MAX)], ..Default::default() };
            let p = pdf(&doc("Hello 中"), &f).unwrap_or_else(|e| panic!("case {i}: {e}"));
            objects_are_where_the_xref_says(&p);
        }
        if let Some(c) = cjk_collection() {
            assert!(sfnt(&FontFile { index: 99, ..c.clone() }).is_none(), "no such face");
            assert!(sfnt(&FontFile { index: 0, ..c }).is_some());
        }
    }

    #[test]
    fn embeds_a_system_font_when_there_is_one() {
        let Some(regular) = noto() else { return };
        let fonts = Fonts { regular: Some(regular.clone()), ..Default::default() };
        let p = pdf(&doc("Hello café — ünïcödé"), &fonts).unwrap();
        let text = String::from_utf8_lossy(&p);
        assert!(text.contains("/Subtype /CIDFontType2") && text.contains("/FontFile2") && text.contains("/ToUnicode"));
        assert!(p.len() < regular.data.len(), "the font file is compressed");
        objects_are_where_the_xref_says(&p);
    }

    /// A table's box drawing, arrows, check marks, stars and CJK come from fallback fonts (never
    /// glyph 0, the box), the emoji selector draws nothing, and each face used has its own
    /// `ToUnicode` map.
    #[test]
    fn fallbacks_draw_what_the_regular_font_lacks() {
        let (Some(regular), Some(symbols)) = (noto(), dejavu()) else { return };
        let mut fallbacks = vec![symbols];
        fallbacks.extend(cjk_collection());
        fallbacks.extend(font(&["/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf"], 0));
        let fonts = Fonts { regular: Some(regular), bold: None, fallbacks };
        let text = "Plan → done ✓ ★ ┼─│ ═ 中文 \u{FE0F}x";
        let missing = fonts.missing(text);
        assert!(!missing.contains(&'→') && !missing.contains(&'┼'), "{missing:?}");
        let (p, report) = pdf_report(&doc(text), &fonts).unwrap();
        assert!(!glyph_ids(&p).iter().any(|g| g == "0000"), "no missing-glyph boxes");
        assert!(report.missing.iter().all(|c| !"→✓★┼─│═".contains(*c)), "{:?}", report.missing);
        let used = count(&p, "/Subtype /Type0");
        assert!(used >= 2, "regular and at least the symbol font: {used}");
        assert_eq!(count(&p, "/ToUnicode"), used, "one map per embedded face");
        objects_are_where_the_xref_says(&p);
        // The variation selector is not a glyph at all.
        let plain = pdf(&doc("x"), &fonts).unwrap();
        let with = pdf(&doc("x\u{FE0F}"), &fonts).unwrap();
        assert_eq!(glyph_ids(&plain), glyph_ids(&with));
    }

    /// Noto Sans CJK is a `.ttc` of CFF fonts: the face is cut out and goes in as `FontFile3`.
    #[test]
    fn cff_faces_of_a_collection_embed_as_fontfile3() {
        let (Some(regular), Some(cjk)) = (noto(), cjk_collection()) else { return };
        let fonts = Fonts { regular: Some(regular), fallbacks: vec![cjk.clone()], ..Default::default() };
        assert!(covers(&cjk, '中') && !covers(&cjk, '\u{1F600}'));
        let (p, report) = pdf_report(&doc("中文 text → ✓"), &fonts).unwrap();
        let text = String::from_utf8_lossy(&p);
        assert!(text.contains("/FontFile3") && text.contains("/Subtype /OpenType") && text.contains("/CIDFontType0"), "CFF goes in whole");
        assert!(!glyph_ids(&p).iter().any(|g| g == "0000") && report.missing.is_empty(), "{:?}", report.missing);
        // One face cut out of the collection (the faces share tables, so it is not small), compressed.
        let face = sfnt(&cjk).unwrap();
        assert!(face.len() < cjk.data.len() && p.len() < face.len());
        objects_are_where_the_xref_says(&p);
    }

    /// A character no font has is drawn as `?`, named in the report; fonts past the cap are left
    /// out, the largest first.
    #[test]
    fn missing_characters_and_the_size_cap() {
        let (Some(regular), Some(symbols)) = (noto(), dejavu()) else { return };
        let fonts = Fonts { regular: Some(regular), fallbacks: vec![symbols], ..Default::default() };
        let (p, report) = pdf_report(&doc("a \u{10FFFD} →"), &fonts).unwrap();
        assert_eq!(report.missing, vec!['\u{10FFFD}']);
        assert!(!glyph_ids(&p).iter().any(|g| g == "0000"));
        let (p, report) = pdf_capped(&doc("a →"), &fonts, 1000).unwrap();
        assert_eq!((report.dropped_fonts, report.missing.clone()), (1, vec!['→']), "{report:?}");
        assert!(!glyph_ids(&p).iter().any(|g| g == "0000"), "drawn as ?, not a box");
    }
}
