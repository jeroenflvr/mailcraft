# Task: a proper Print dialog (printer options like Outlook's)

Repository: `~/projects/mailcraft`. Read `AGENTS.md` first and follow it (no panics, everything
is a command, `tl!`/`tf!` for every string, `cargo xtask ci` before each commit, commit after each
arc). Work autonomously; the only owner decisions are listed at the end.

## Why

`Print` (reading pane › Print, File › Print, `message.print`) opens `Dialog::Print`
(`crates/ui-egui/src/dialogs.rs`, around line 2143): a printer list and a copies spinner, nothing
else. Outlook's Print page offers the usual printer options. Users expect at least orientation,
double-sided, colour/black-and-white, copies (collated), paper size, scaling and a page range.
Since v0.19.1 the PDF is laid out as the reading pane shows the message (`crates/print`), so the
options now matter.

## What exists (read these before changing anything)

- `crates/model/src/lib.rs`: `Printer { name, default }`, `Printing { None | Choose | Viewer }`.
- `crates/engine/src/session.rs`: `PrintRequest { doc, printer, copies }`;
  `Session::take_print_requests()`. `crates/engine/src/cmd.rs`: `message.print`
  (`{"printer", "copies"}`), `message.exportPdf`, `message_doc()`, `render_pdf()`.
- `crates/print/src/lib.rs`: `PAGE_W`/`PAGE_H` are A4 constants, `MARGIN` 56 pt, portrait only;
  `pdf_report(doc, fonts)`; `layout.rs` lays the body into one tall column `page_h` high and
  `lib.rs` cuts it into pages. Colours are written as `rg`/`RG` operands.
- `crates/net/src/print.rs`: `printers()` (`lpstat -p -d`), `print_pdf(printer, copies, pdf)`
  (`lp -d P -n N -t title file`; Windows: PowerShell `Start-Process -Verb Print`, repeated per
  copy). Tests run fake `lpstat`/`lp` scripts; keep that pattern, never touch a real printer.
- `crates/ui-egui/src/lib.rs`: `Services` trait (`printers`, `print_pdf`, `print_fonts`),
  `Action::Print | QuickPrint | OpenAsPdf | ExportPdf`, the request loop that renders the PDF
  and calls `print_pdf` (~line 680). `NullServices` in tests. `apps/mailcraft/src/main.rs`
  implements `Services` over `mailcraft_net`. `apps/mailcraft-cli/src/main.rs` has
  `print KEY [--printer] [--copies]`.
- `crates/ui-egui/src/tests.rs` ~line 560–600: headless tests of the dialog and Quick print.
- Docs: `docs/gaps.md` ("v0.19 — printing…" section), `docs/architecture.md` (print rows),
  `ROADMAP.md`.

## Design

### 1. The options model (L0, `crates/model/src/lib.rs`)

```rust
pub struct PrintOptions {
    pub printer: Option<String>,      // None: the system's default
    pub copies: u32,                  // 1..=99
    pub collate: bool,                // default true
    pub orientation: Orientation,     // Portrait | Landscape
    pub duplex: Duplex,               // Off | LongEdge | ShortEdge
    pub color: ColorMode,             // Color | Monochrome
    pub paper: Paper,                 // A4 | Letter | Legal | A3 | A5 (sizes in points inside `print`)
    pub scale: Scale,                 // FitToPage (default) | Percent(u16 25..=400)
    pub pages: PageRange,             // All | Range(Vec<(u32,u32)>) parsed from "1-3, 5"
    pub headers: bool,                // the subject/From/To/Date block (default true)
}
```
Serde with defaults on every field so old `message.print` params still work. `Default` is what
the dialog opens with. `PageRange::parse(&str) -> Result<_, String>` with tests (bounds, order,
junk).

Keep `Printer` and add what `lpstat`/`lpoptions` can tell us (all `Option`/`bool`, default off):
`duplex: bool`, `color: bool` (printer supports), `papers: Vec<Paper>`. The dialog greys out what
the printer lacks; unknown means "offer it".

### 2. The print crate (`crates/print`)

- `pdf_report(doc, fonts)` → `pdf_report_with(doc, fonts, &PageSetup)` where
  `PageSetup { paper, orientation, scale, pages, headers }` (a `print`-side type built from
  `PrintOptions`; keep the old function as a thin wrapper = A4 portrait, fit, all pages). Replace
  the `PAGE_W`/`PAGE_H` constants with the setup's width/height (swap for landscape); `MARGIN`
  stays. `Scale::Percent(p)`: lay out into a column `width / s` wide, then write a `cm` matrix
  scaling the content by `s`; `FitToPage` = 100% (content already fits the width). `pages`: emit
  only the listed pages but keep "n / total" numbering honest (use the total). `headers: false`
  skips the title/header block and the rule.
- Add `/MediaBox` per setup; set `/Rotate 0` (don't rotate; really lay out landscape).
- Monochrome is a CUPS job option, not a PDF concern (below). Do **not** desaturate the PDF.
- Tests: landscape swaps the MediaBox; a 50% scale puts twice as many lines on a page as 100%;
  a range of `2-3` of a 5-page doc yields 2 pages whose footers read `2 / 5` and `3 / 5`;
  `headers: false` has no rule; junk setups (0% scale, range past the end, empty range) never
  panic. Use the existing `page_texts`/`placed` helpers.

### 3. Sending (`crates/net/src/print.rs`)

- `print_pdf(options: &PrintOptions, pdf: &[u8])`. Map to `lp -o` (CUPS options, documented in
  the CUPS manual, not copied from anywhere): `-n N`, `-o collate=true|false`,
  `-o landscape` (only when the PDF isn't already landscape — since we lay out landscape, pass
  `-o orientation-requested=3`/`4` only if the printer needs it; simplest: don't pass
  orientation at all, the PDF carries it; test both ways with a real CUPS if available),
  `-o sides=one-sided|two-sided-long-edge|two-sided-short-edge`,
  `-o print-color-mode=monochrome|color`, `-o media=A4|Letter|Legal|A3|A5`,
  `-o page-ranges=1-3,5` (only if we didn't already cut the PDF — pick one: cut in the PDF,
  which also works on Windows, and don't pass `page-ranges`), `-o fit-to-page` when
  `Scale::FitToPage` and the paper differs from the PDF's (otherwise omit).
- Every option value is from a closed enum or a clamped number: never interpolate free text into
  `lp` args (the printer name check `valid()` stays).
- Printer capabilities: `lpoptions -p NAME -l` lists `Duplex/…`, `ColorModel/…`, `PageSize/…`
  with the current value starred. Parse defensively (missing command → no capabilities, not an
  error). Add fake-script tests like the existing ones for `lpstat`.
- Windows: the Print verb can't take options; keep copies by repetition, and tell the user in
  the dialog that the PDF app's dialog decides the rest (orientation, paper and page range still
  apply because they're in the PDF).
- Web (`Printing::None`): the dialog becomes a **Page setup** for Save as PDF: orientation,
  paper, scale, pages, headers; printer/copies/duplex/colour hidden.

### 4. Engine (`crates/engine`)

- `message.print` params become a `PrintOptions` JSON object (flat, all optional); keep
  `{"printer","copies"}` working. `PrintRequest { doc, options }`.
- `message.exportPdf` gains the same page-setup fields (optional) → passes a `PageSetup`.
- Remember the last options in `Prefs` (`prefs.print: PrintOptions`, serde default) except the
  page range; the dialog opens with them (Outlook remembers printer and settings per session;
  we remember across runs). Document in `every_command_survives_junk_params` that junk is fine
  (it already runs every command with junk; make sure the new parsing is `Result`, never panics).

### 5. The dialog (`crates/ui-egui/src/dialogs.rs`)

Layout like Outlook's Print page, simplified, left column settings, right column a preview:

- **Printer** combo (default marked) · *Printer properties* not offered.
- **Copies** DragValue 1–99 · ☑ Collate (only when copies > 1).
- **Pages**: ◉ All ◯ Pages: `[ 1-3, 5 ]` text field, validated live (red note on error).
- **Orientation**: Portrait / Landscape (two icon buttons drawn in code: `icons.rs`).
- **Paper**: combo of what the printer offers (or the five sizes).
- **Print on both sides**: Off / Long edge / Short edge (greyed when the printer can't).
- **Colour**: Colour / Black and white (greyed when the printer can't).
- **Scale**: Fit to page / 100% / 75% / 50% / custom %.
- ☑ Print the message header (subject, From, To, Date).
- **Preview** on the right: render page 1 of the PDF with the current setup into a texture.
  There's a PDF thumbnail path already for attachments (`thumbs.rs`, first page); reuse it. Debounce
  (re-render only when options change; cap at ~300 ms budget; show a spinner meanwhile). Show
  "Page 1 of N".
- Footer: **Print**, **Cancel**, link *Save as PDF instead* (opens Save As with the same setup).
- Keyboard: Enter prints, Esc cancels (see the Enter fix noted in gaps.md v0.18 — don't repeat
  the focus bug).
- `Quick print` (list menu) keeps printing straight to the default printer with the remembered
  options.
- Every new string in every `i18n/*.tsv` (the tests fail otherwise). Use the existing style:
  short, Outlook's feature names only.

### 6. CLI

`mailcraft-cli print KEY [--printer N] [--copies N] [--landscape] [--duplex long|short]
[--mono] [--paper a4|letter|…] [--scale fit|NN] [--pages 1-3,5] [--no-header]`; same flags for a
new `mailcraft-cli pdf KEY OUT.pdf`. Update `help.txt`.

### 7. Tests and gates

- `crates/print`: as in §2.
- `crates/net`: fake `lp` script that echoes its args to a file; assert the exact `-o` list for a
  few option sets; fake `lpoptions` output parsed into capabilities; missing tools → defaults.
- `crates/ui-egui/src/tests.rs`: the dialog draws in light/dark with every `Printing` variant,
  with a printer lacking duplex/colour (controls disabled), with an invalid page range (Print
  disabled); Enter submits; submitted JSON carries the options; preview doesn't panic without a
  renderer.
- `crates/engine`: `message.print` with old params, with full options, with junk; prefs round-trip.
- `cargo xtask ci` green; `cargo xtask wasm` green (the web build only sees `model`/`print`).

### 8. Docs

`docs/gaps.md` new "Unreleased — the Print dialog" entry (what's in, what isn't: no printer
properties, Windows options go through the PDF app, no booklet/n-up), `docs/architecture.md`
print rows, `ROADMAP.md` dated line, `apps/mailcraft-cli/src/help.txt`.

## Order of work (commit after each)

1. `model::PrintOptions` + `PageRange::parse` + tests.
2. `print`: `PageSetup`, landscape, paper, scale, page cut, headers toggle + tests.
3. `net::print`: options → `lp -o`, `lpoptions` capabilities + fake-script tests.
4. `engine`: params, `PrintRequest`, prefs; CLI flags.
5. UI dialog without preview + i18n + headless tests.
6. Preview.
7. Docs.

## Not in scope / owner decisions

- No printer-driver properties dialog, no n-up/booklet, no Windows printer selection (would need
  `unsafe` or the `windows` crate — forbidden).
- Don't add dependencies for PDF rendering beyond what `thumbs.rs` already uses; if the preview
  can't be done with what's there, ship without it and say so in gaps.md.
- Never print to a real printer from tests or while checking your work; use the fake scripts and
  look at the PDF (`pdftoppm` is installed) instead.
