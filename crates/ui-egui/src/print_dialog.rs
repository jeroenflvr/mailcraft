//! The Print dialog (File › Print): the printer's options on the left, the page as it will print
//! on the right. Where nothing prints from the app (the browser) it is a page setup for Save as
//! PDF: orientation, paper, scale, pages and the header.
//!
//! The dialog only asks, and is drawn at once with whatever is known. The printers, what the
//! chosen one can do, the fonts and the preview come in later, each on its own, and a spinner
//! shows where they will be. The preview is rendered by the app ([`Action::PrintPreview`], on
//! another thread) once the settings have rested for a moment, and the answer is left in a slot
//! that [`Preview::update`] picks up.

use std::sync::{Arc, Mutex, PoisonError};

use egui::{Align2, Rect, RichText, Sense, Ui, vec2};
use mailcraft_engine::model::{Capabilities, ColorMode, Duplex, Orientation, PageRange, Paper, PrintOptions, Printer, Printing, Quality, Scale};
use mailcraft_engine::print::Fonts;
use serde_json::{Value, json};

use crate::icons::{self, Icon};
use crate::theme::Tokens;
use crate::{Action, dialogs};

/// How long the settings rest before the preview is drawn again.
const PREVIEW_AFTER: f64 = 0.15;
/// The preview is drawn this many pixels wide (the dialog shows it smaller, sharper on dense screens).
pub(crate) const PREVIEW_PX: u32 = 520;

/// What the preview shows, so it is drawn again only when something that changes it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviewKey {
    paper: Paper,
    orientation: Orientation,
    scale: u16,
    headers: bool,
    /// The first page asked for (1 for all).
    first: u32,
}

impl PreviewKey {
    /// The number of the first page asked for (1 for all of them).
    pub(crate) fn first_page(&self) -> u32 {
        self.first
    }

    pub(crate) fn of(o: &PrintOptions) -> PreviewKey {
        let first = match &o.pages {
            PageRange::All => 1,
            PageRange::Range(r) => r.first().map_or(1, |&(a, _)| a.max(1)),
        };
        PreviewKey { paper: o.paper, orientation: o.orientation, scale: o.scale.percent(), headers: o.headers, first }
    }
}

/// What a preview job leaves behind: the settings it drew, the fonts it found (if it had to look),
/// and the page as a picture with the page count and the number of the page shown.
pub(crate) type PreviewResult = (PreviewKey, Option<Fonts>, Result<(egui::ColorImage, usize, usize), String>);

/// Where a preview job leaves its answer.
pub(crate) type PreviewSlot = Arc<Mutex<Option<PreviewResult>>>;

/// The page as it will print: asked for after the settings rest, shown when the app has drawn it.
#[derive(Clone, Default)]
pub struct Preview {
    /// The preview being drawn on another thread (one at a time).
    in_flight: Option<PreviewSlot>,
    /// The fonts it is drawn with, asked of the platform once.
    pub(crate) fonts: Option<Fonts>,
    wanted: Option<PreviewKey>,
    requested: Option<PreviewKey>,
    shown: Option<PreviewKey>,
    changed_at: f64,
    fresh: Option<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    /// The pages the message takes on this paper, once known.
    pub(crate) total: Option<usize>,
    /// The number of the page shown.
    page: usize,
    failed: bool,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview").field("total", &self.total).field("page", &self.page).field("drawn", &self.texture.is_some()).finish()
    }
}

impl Preview {
    /// The app's answer: the page as a picture, how many pages the message has, and which one it is.
    pub(crate) fn finish(&mut self, key: PreviewKey, result: Result<(egui::ColorImage, usize, usize), String>) {
        self.requested = None;
        self.shown = Some(key);
        match result {
            Ok((image, total, page)) => {
                self.fresh = Some(image);
                self.total = Some(total);
                self.page = page;
                self.failed = false;
            }
            Err(_) => {
                self.fresh = None;
                self.texture = None;
                self.failed = true;
            }
        }
    }

    /// A picture is being drawn, answered in `slot`.
    pub(crate) fn started(&mut self, slot: PreviewSlot) {
        self.in_flight = Some(slot);
    }

    /// Ask for a new picture when the settings changed and have rested (the first one started at
    /// once), and take the one that has been drawn.
    fn update(&mut self, ctx: &egui::Context, key: PreviewKey, options: &PrintOptions, acts: &mut Vec<Action>) {
        let now = ctx.input(|i| i.time);
        if self.wanted.as_ref() != Some(&key) {
            self.wanted = Some(key);
            self.changed_at = now;
        }
        if let Some(slot) = &self.in_flight {
            let answer = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
            match answer {
                Some((key, fonts, result)) => {
                    self.in_flight = None;
                    if fonts.is_some() {
                        self.fonts = fonts;
                    }
                    self.finish(key, result);
                }
                None => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        if let Some(image) = self.fresh.take() {
            self.texture = Some(ctx.load_texture("print-preview", image, egui::TextureOptions::LINEAR));
        }
        // One at a time: a newer wish waits for the one being drawn.
        if self.shown != self.wanted && self.requested != self.wanted && self.in_flight.is_none() {
            if self.shown.is_none() || now - self.changed_at >= PREVIEW_AFTER {
                self.requested = self.wanted.clone();
                acts.push(Action::PrintPreview(Box::new(options.clone())));
            } else {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(PREVIEW_AFTER));
            }
        }
    }

    /// Whether a picture of the page is shown.
    #[cfg(test)]
    pub(crate) fn drawn(&self) -> bool {
        self.texture.is_some()
    }

    fn pending(&self) -> bool {
        self.shown != self.wanted && !self.failed
    }
}

/// The scale choices of the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScaleKind {
    Fit,
    P100,
    P75,
    P50,
    Custom,
}

/// What is wrong with the page list.
enum Problem {
    /// Not a list of pages.
    Syntax,
    /// Pages the message doesn't have (it has this many).
    Past(usize),
}

/// The system's printers, as far as the dialog knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrinterList {
    /// Being asked.
    Loading,
    Ready(Vec<Printer>),
    /// Why they couldn't be listed (no print system, its scheduler isn't running).
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PrintForm {
    pub printing: Printing,
    pub printers: PrinterList,
    /// The printers whose capabilities have been answered (or given up on).
    probed: Vec<String>,
    /// The time the dialog was asked for, until its first frame has been timed.
    pub(crate) opened_at: Option<f64>,
    /// What the dialog has chosen; the printer is the chosen one's name.
    pub options: PrintOptions,
    pub(crate) range_all: bool,
    pub(crate) range_text: String,
    scale: ScaleKind,
    custom: u16,
    pub(crate) preview: Preview,
    /// What the print command said was wrong.
    pub error: Option<String>,
}

impl ScaleKind {
    const ALL: [ScaleKind; 5] = [ScaleKind::Fit, ScaleKind::P100, ScaleKind::P75, ScaleKind::P50, ScaleKind::Custom];

    fn label(self, custom: u16) -> String {
        match self {
            ScaleKind::Fit => tl!("Fit to page").to_string(),
            ScaleKind::P100 => "100%".into(),
            ScaleKind::P75 => "75%".into(),
            ScaleKind::P50 => "50%".into(),
            ScaleKind::Custom => format!("{} {custom}%", tl!("Custom")),
        }
    }

    fn of(scale: Scale) -> (ScaleKind, u16) {
        match scale {
            Scale::FitToPage => (ScaleKind::Fit, 100),
            Scale::Percent(100) => (ScaleKind::P100, 100),
            Scale::Percent(75) => (ScaleKind::P75, 75),
            Scale::Percent(50) => (ScaleKind::P50, 50),
            Scale::Percent(p) => (ScaleKind::Custom, p),
        }
    }

    fn scale(self, custom: u16) -> Scale {
        match self {
            ScaleKind::Fit => Scale::FitToPage,
            ScaleKind::P100 => Scale::Percent(100),
            ScaleKind::P75 => Scale::Percent(75),
            ScaleKind::P50 => Scale::Percent(50),
            ScaleKind::Custom => Scale::Percent(custom),
        }
    }
}

impl PrintForm {
    /// The dialog as it opens: the last settings, and the printers still to come
    /// ([`PrintForm::printers_arrived`]). The printer they used is kept as the one to choose.
    pub fn new(printing: Printing, saved: &PrintOptions) -> PrintForm {
        let mut options = PrintOptions { pages: PageRange::All, ..saved.clone() }.normalized();
        if printing != Printing::Choose {
            options.printer = None;
        }
        let (scale, custom) = ScaleKind::of(options.scale);
        PrintForm {
            printing,
            printers: if printing == Printing::Choose { PrinterList::Loading } else { PrinterList::Ready(Vec::new()) },
            probed: Vec::new(),
            opened_at: None,
            options,
            range_all: true,
            range_text: String::new(),
            scale,
            custom,
            preview: Preview::default(),
            error: None,
        }
    }

    /// The printers listed, if they have been.
    fn list(&self) -> &[Printer] {
        match &self.printers {
            PrinterList::Ready(p) => p,
            _ => &[],
        }
    }

    /// Whether the printers are still being asked for.
    pub fn loading(&self) -> bool {
        self.printers == PrinterList::Loading
    }

    /// The printers whose capabilities are still to come.
    pub fn unprobed(&self) -> Vec<String> {
        self.list().iter().filter(|p| !self.probed.contains(&p.name)).map(|p| p.name.clone()).collect()
    }

    /// Whether the chosen printer is still being asked what it can do.
    fn asking(&self) -> bool {
        self.options.printer.as_ref().is_some_and(|n| self.list().iter().any(|p| &p.name == n) && !self.probed.contains(n))
    }

    /// The system's answer about its printers. The default printer is chosen; without one, the
    /// last used if it is still there, else none (Print waits for a choice). Nothing else the user
    /// set is touched. The same answer again changes nothing.
    pub fn printers_arrived(&mut self, result: Result<Vec<Printer>, String>) {
        match result {
            Ok(mut list) => {
                if let PrinterList::Ready(old) = &self.printers {
                    let same = |a: &Printer, b: &Printer| a.name == b.name && a.default == b.default;
                    if old.len() == list.len() && old.iter().zip(&list).all(|(a, b)| same(a, b)) {
                        return;
                    }
                    for p in &mut list {
                        if let Some(o) = old.iter().find(|o| o.name == p.name) {
                            (p.duplex, p.color, p.papers) = (o.duplex, o.color, o.papers.clone());
                        }
                    }
                }
                self.probed.retain(|n| list.iter().any(|p| &p.name == n));
                // The system's default wins when the dialog opens. Later (a refreshed list) the
                // printer in use stays; one that has gone, or none, leaves nothing chosen but the default.
                let first_answer = !matches!(self.printers, PrinterList::Ready(_));
                let default = list.iter().find(|p| p.default);
                let current = self.options.printer.as_ref().and_then(|n| list.iter().find(|p| &p.name == n));
                let chosen = if first_answer { default.or(current) } else { current.or(default) };
                self.options.printer = chosen.map(|p| p.name.clone());
                self.printers = PrinterList::Ready(list);
                self.fit_to_printer();
            }
            Err(e) => {
                if self.printers != PrinterList::Failed(e.clone()) {
                    self.options.printer = None;
                    self.printers = PrinterList::Failed(e);
                }
            }
        }
    }

    /// The system's answer about one printer (`None`: it couldn't be asked, so all is offered).
    pub fn capabilities_arrived(&mut self, name: &str, caps: Option<Capabilities>) {
        if !self.probed.iter().any(|n| n == name) {
            self.probed.push(name.to_string());
        }
        if let (Some(caps), PrinterList::Ready(list)) = (caps, &mut self.printers)
            && let Some(p) = list.iter_mut().find(|p| p.name == name)
        {
            p.learn(caps);
        }
        if self.options.printer.as_deref() == Some(name) {
            self.fit_to_printer();
        }
    }

    fn printer(&self) -> Option<&Printer> {
        let name = self.options.printer.as_ref()?;
        self.list().iter().find(|p| &p.name == name)
    }

    pub(crate) fn can_duplex(&self) -> bool {
        self.printer().is_none_or(|p| p.duplex != Some(false))
    }

    pub(crate) fn can_color(&self) -> bool {
        self.printer().is_none_or(|p| p.color != Some(false))
    }

    /// The paper sizes offered: the printer's, or all of them when it didn't say.
    fn papers(&self) -> Vec<Paper> {
        match self.printer() {
            Some(p) if !p.papers.is_empty() => p.papers.clone(),
            _ => Paper::ALL.to_vec(),
        }
    }

    /// What the printer lacks isn't left chosen.
    pub(crate) fn fit_to_printer(&mut self) {
        if !self.can_duplex() {
            self.options.duplex = Duplex::Off;
        }
        if !self.can_color() {
            self.options.color = ColorMode::Monochrome;
        }
        let papers = self.papers();
        if !papers.contains(&self.options.paper)
            && let Some(first) = papers.first()
        {
            self.options.paper = *first;
        }
    }

    /// The pages asked for, or what is wrong with them.
    fn pages(&self) -> Result<PageRange, Problem> {
        if self.range_all {
            return Ok(PageRange::All);
        }
        let range = match PageRange::parse(&self.range_text) {
            Ok(PageRange::All) | Err(_) => return Err(Problem::Syntax),
            Ok(r) => r,
        };
        match self.preview.total {
            Some(total) if range.pages(u32::try_from(total).unwrap_or(u32::MAX)).is_empty() => Err(Problem::Past(total)),
            _ => Ok(range),
        }
    }

    /// Everything chosen, when the page list is a good one.
    fn chosen(&self) -> Result<PrintOptions, Problem> {
        Ok(PrintOptions { pages: self.pages()?, scale: self.scale.scale(self.custom), ..self.options.clone() }.normalized())
    }

    /// What the preview is drawn with: a page list that isn't one means all of the pages.
    fn for_preview(&self) -> PrintOptions {
        self.chosen()
            .unwrap_or_else(|_| PrintOptions { pages: PageRange::All, scale: self.scale.scale(self.custom), ..self.options.clone() }.normalized())
    }

    /// The params of the command that prints, or saves the PDF.
    fn params(options: &PrintOptions, remember: bool) -> Value {
        let mut v = serde_json::to_value(options).unwrap_or_else(|_| json!({}));
        v["remember"] = json!(remember);
        v
    }
}

/// A button with a drawn icon that is on or off (portrait, landscape).
fn choice(ui: &mut Ui, t: &Tokens, icon: Icon, label: &str, on: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(104.0, 28.0), Sense::click());
    let fill = if on {
        t.accent_soft
    } else if response.hovered() {
        t.hover
    } else {
        egui::Color32::TRANSPARENT
    };
    let stroke = if on { t.accent } else { t.border };
    ui.painter().rect(rect, 4.0, fill, egui::Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    icons::paint(ui.painter(), Rect::from_min_size(rect.min + vec2(8.0, 6.0), vec2(16.0, 16.0)), icon, t.text);
    ui.painter().text(rect.min + vec2(32.0, 14.0), Align2::LEFT_CENTER, label, egui::TextStyle::Body.resolve(ui.style()), t.text);
    response
}

fn setting(ui: &mut Ui, t: &Tokens, label: &str) {
    ui.label(RichText::new(label).color(t.dim).small());
}

fn note(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(t.dim).small()).wrap());
}

/// The dialog; whether it stays open.
pub fn body(ui: &mut Ui, f: &mut PrintForm, t: &Tokens, acts: &mut Vec<Action>) -> bool {
    let choosing = f.printing == Printing::Choose;
    let saving = f.printing == Printing::None;
    ui.set_width(if saving { 600.0 } else { 680.0 });
    if saving {
        dialogs::title(ui, t, "Page setup");
    } else {
        dialogs::title(ui, t, "Print");
    }
    // Until the printers are listed, and without any, nothing can be sent; the PDF is still offered.
    let loading = choosing && f.loading();
    let no_printers = choosing && !loading && f.list().is_empty();
    let can = !choosing || !f.list().is_empty();

    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(if saving { 330.0 } else { 390.0 });
            match f.printing {
                Printing::Viewer => {
                    note(ui, t, tl!("The PDF app asks which printer to use."));
                    note(ui, t, tl!("The PDF app's print dialog decides the rest, such as both sides and color."));
                    ui.add_space(6.0);
                }
                Printing::Choose if no_printers => {
                    note(ui, t, tl!("No printers were found. Open the message as a PDF and print it from the viewer."));
                    if let PrinterList::Failed(w) = &f.printers {
                        ui.add_space(4.0);
                        ui.label(RichText::new(w.as_str()).color(t.danger).small());
                    }
                    ui.add_space(6.0);
                }
                _ => {}
            }
            egui::Grid::new("print-settings").num_columns(2).spacing(vec2(12.0, 9.0)).show(ui, |ui| {
                if loading {
                    setting(ui, t, tl!("Printer"));
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(RichText::new(tl!("Looking for printers…")).color(t.dim).small());
                    });
                    ui.end_row();
                } else if choosing && can {
                    setting(ui, t, tl!("Printer"));
                    let selected = f.options.printer.clone().unwrap_or_default();
                    let label = match f.list().iter().find(|p| p.name == selected) {
                        Some(p) if p.default => tf!("{name} (default)", name = p.name),
                        Some(_) => selected,
                        None => tl!("Choose a printer").to_string(),
                    };
                    let before = f.options.printer.clone();
                    let list = f.list().to_vec();
                    egui::ComboBox::from_id_salt("print-printer").width(240.0).selected_text(label).show_ui(ui, |ui| {
                        for p in &list {
                            let text = if p.default { tf!("{name} (default)", name = p.name) } else { p.name.clone() };
                            ui.selectable_value(&mut f.options.printer, Some(p.name.clone()), text);
                        }
                    });
                    if f.options.printer != before {
                        f.fit_to_printer();
                    }
                    ui.end_row();
                }
                if f.printing != Printing::None {
                    setting(ui, t, tl!("Copies"));
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut f.options.copies).range(1..=99));
                        if choosing {
                            ui.add_enabled(f.options.copies > 1, egui::Checkbox::new(&mut f.options.collate, tl!("Collate")));
                        }
                    });
                    ui.end_row();
                }

                setting(ui, t, tl!("Pages"));
                ui.vertical(|ui| {
                    ui.radio_value(&mut f.range_all, true, tl!("All"));
                    ui.horizontal(|ui| {
                        if ui.radio(!f.range_all, tl!("Pages:")).clicked() {
                            f.range_all = false;
                        }
                        let edit = ui.add(egui::TextEdit::singleline(&mut f.range_text).hint_text("1-3, 5").desired_width(120.0));
                        // Typing pages chooses them; emptying the field goes back to all.
                        if edit.changed() {
                            f.range_all = f.range_text.trim().is_empty();
                        }
                    });
                    match f.pages() {
                        Err(Problem::Syntax) if !f.range_text.trim().is_empty() => {
                            ui.label(RichText::new(tl!("Use page numbers like 1-3, 5")).color(t.danger).small());
                        }
                        Err(Problem::Past(total)) => {
                            ui.label(RichText::new(tf!("The message has only {total} pages", total = total)).color(t.danger).small());
                        }
                        _ => {}
                    }
                });
                ui.end_row();

                setting(ui, t, tl!("Orientation"));
                ui.horizontal(|ui| {
                    if choice(ui, t, Icon::Portrait, tl!("Portrait"), f.options.orientation == Orientation::Portrait).clicked() {
                        f.options.orientation = Orientation::Portrait;
                    }
                    if choice(ui, t, Icon::Landscape, tl!("Landscape"), f.options.orientation == Orientation::Landscape).clicked() {
                        f.options.orientation = Orientation::Landscape;
                    }
                });
                ui.end_row();

                setting(ui, t, tl!("Paper"));
                let papers = f.papers();
                egui::ComboBox::from_id_salt("print-paper").width(140.0).selected_text(f.options.paper.code()).show_ui(ui, |ui| {
                    for p in papers {
                        ui.selectable_value(&mut f.options.paper, p, p.code());
                    }
                });
                ui.end_row();

                if choosing {
                    setting(ui, t, tl!("Print on both sides"));
                    let ok = f.can_duplex();
                    let r = ui
                        .add_enabled_ui(ok, |ui| {
                            let shown = match f.options.duplex {
                                Duplex::Off => tl!("Off"),
                                Duplex::LongEdge => tl!("Long edge"),
                                Duplex::ShortEdge => tl!("Short edge"),
                            };
                            egui::ComboBox::from_id_salt("print-duplex").width(140.0).selected_text(shown).show_ui(ui, |ui| {
                                ui.selectable_value(&mut f.options.duplex, Duplex::Off, tl!("Off"));
                                ui.selectable_value(&mut f.options.duplex, Duplex::LongEdge, tl!("Long edge"));
                                ui.selectable_value(&mut f.options.duplex, Duplex::ShortEdge, tl!("Short edge"));
                            });
                        })
                        .response;
                    if !ok {
                        r.on_hover_text(tl!("Not offered by this printer"));
                    }
                    ui.end_row();

                    setting(ui, t, tl!("Color"));
                    let ok = f.can_color();
                    let r = ui
                        .add_enabled_ui(ok, |ui| {
                            let shown = match f.options.color {
                                ColorMode::Color => tl!("Color"),
                                ColorMode::Monochrome => tl!("Black and white"),
                            };
                            egui::ComboBox::from_id_salt("print-color").width(140.0).selected_text(shown).show_ui(ui, |ui| {
                                ui.selectable_value(&mut f.options.color, ColorMode::Color, tl!("Color"));
                                ui.selectable_value(&mut f.options.color, ColorMode::Monochrome, tl!("Black and white"));
                            });
                        })
                        .response;
                    if !ok {
                        r.on_hover_text(tl!("Not offered by this printer"));
                    }
                    ui.end_row();
                }

                if choosing {
                    setting(ui, t, tl!("Quality"));
                    let name = |q: Quality| match q {
                        Quality::Draft => tl!("Draft"),
                        Quality::Normal => tl!("Normal"),
                        Quality::High => tl!("High"),
                    };
                    egui::ComboBox::from_id_salt("print-quality").width(140.0).selected_text(name(f.options.quality)).show_ui(ui, |ui| {
                        for q in Quality::ALL {
                            ui.selectable_value(&mut f.options.quality, q, name(q));
                        }
                    });
                    ui.end_row();
                }

                setting(ui, t, tl!("Scale"));
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("print-scale").width(140.0).selected_text(f.scale.label(f.custom)).show_ui(ui, |ui| {
                        for k in ScaleKind::ALL {
                            ui.selectable_value(&mut f.scale, k, k.label(f.custom));
                        }
                    });
                    if f.scale == ScaleKind::Custom {
                        ui.add(egui::DragValue::new(&mut f.custom).range(25..=400).suffix("%"));
                    }
                });
                ui.end_row();
            });
            if choosing && f.asking() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(RichText::new(tl!("Asking the printer…")).color(t.dim).small());
                });
            }
            ui.add_space(8.0);
            ui.checkbox(&mut f.options.headers, tl!("Print the message header"));
        });

        ui.add_space(14.0);
        ui.vertical(|ui| {
            ui.set_width(250.0);
            setting(ui, t, tl!("Preview"));
            ui.add_space(2.0);
            let options = f.for_preview();
            f.preview.update(ui.ctx(), PreviewKey::of(&options), &options, acts);
            match &f.preview.texture {
                Some(tex) => {
                    let [w, h] = tex.size();
                    let k = (250.0 / w.max(1) as f32).min(330.0 / h.max(1) as f32);
                    let r = ui.add(egui::Image::new((tex.id(), vec2(w as f32 * k, h as f32 * k))));
                    ui.painter().rect_stroke(r.rect, 0.0, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Outside);
                }
                None => {
                    ui.allocate_ui(vec2(250.0, 200.0), |ui| {
                        ui.centered_and_justified(|ui| {
                            if f.preview.failed {
                                ui.label(RichText::new(tl!("No preview")).color(t.dim));
                            } else {
                                ui.add(egui::Spinner::new());
                            }
                        });
                    });
                }
            }
            ui.horizontal(|ui| {
                if let (Some(total), false) = (f.preview.total, f.preview.failed) {
                    ui.label(RichText::new(tf!("Page {page} of {total}", page = f.preview.page, total = total)).color(t.dim).small());
                }
                if f.preview.pending() && f.preview.texture.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
        });
    });

    if let Some(e) = &f.error {
        ui.add_space(6.0);
        ui.add(egui::Label::new(RichText::new(e.as_str()).color(t.danger).small()).wrap());
    }

    // Enter prints, unless a field is being edited (the page list ends its edit with it).
    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && ui.memory(|m| m.focused().is_none());

    ui.add_space(6.0);
    let chosen = f.chosen();
    let mut as_pdf = false;
    if no_printers {
        as_pdf = ui.link(tl!("Open as a PDF instead")).clicked();
    } else if !saving {
        as_pdf = ui.link(tl!("Save as PDF instead")).clicked();
    }
    let ok_enabled = (can || saving) && chosen.is_ok() && (!choosing || f.options.printer.is_some());
    let (ok, cancel) = if saving { dialogs::buttons(ui, t, "Save as PDF", ok_enabled) } else { dialogs::buttons(ui, t, "Print", ok_enabled && can) };

    if as_pdf {
        if no_printers {
            acts.push(Action::OpenAsPdf);
        } else {
            acts.push(Action::SavePdf(Box::new(PrintForm::params(&f.for_preview(), true))));
        }
        return false;
    }
    if let Some(opened) = f.opened_at.take() {
        log::debug!("print: dialog open in {:.0} ms", (ui.input(|i| i.time) - opened).max(0.0) * 1000.0);
    }
    if (ok || enter)
        && ok_enabled
        && !cancel
        && let Ok(options) = chosen
    {
        if saving {
            acts.push(Action::SavePdf(Box::new(PrintForm::params(&options, true))));
            return false;
        }
        f.error = None;
        acts.push(Action::Submit("message.print", PrintForm::params(&options, true)));
    }
    !cancel
}
