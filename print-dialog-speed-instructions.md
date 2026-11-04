# Task: the Print dialog opens at once; every slow part loads on its own, with a spinner

Repository: `~/projects/mailcraft`. Read `AGENTS.md` first and follow it (no panics, no `unsafe`,
everything is a command, `tl!`/`tf!` for every string, `cargo xtask ci` before each commit,
commit after each arc). Work autonomously; there are no owner decisions in this task.

## Why

File › Print (and the reading pane's Print, `message.print`) takes up to **five seconds** before
anything shows. The dialog itself is fine; it is built only after everything it could ever need
has been fetched, on the UI thread, so the window freezes and the user sees nothing. The rest of
the app loads every slow thing in the background and shows a spinner meanwhile (video
thumbnails, attachment previews, sync); the Print dialog must do the same. **This is a
regression in feel and outranks any print feature work.**

## Where the time goes (verified in the code — don't re-investigate, fix)

1. **Listing printers, before the dialog exists.** `crates/ui-egui/src/lib.rs`, `Action::Print`
   (~line 1396):
   ```rust
   let printers = if printing == Printing::Choose { self.services.printers() } else { Ok(Vec::new()) };
   self.ui.dialog = Some(Dialog::print(printing, printers, &self.session.prefs.print));
   ```
   `Services::printers()` → `mailcraft_net::print::printers()` → `list()`
   (`crates/net/src/print.rs` ~line 118): runs `lpstat -p -d` **and then `lpoptions -p NAME -l`
   once per printer** (up to `MAX_PROBED` = 50), each a child process that waits on the CUPS
   scheduler. A remote or sleepy `cupsd`, a network printer that is off, or several printers,
   and this alone is seconds. Nothing has a timeout.
2. **The first preview, inline.** `MailApp::print_preview` (`lib.rs` ~line 690) is run from
   `act(Action::PrintPreview)` on the UI thread and does, in sequence:
   - `fonts_for_open_message()` → `Services::print_fonts` → `mailcraft_net::fonts::print_fonts`:
     the **first** call builds the system font `Collection` (`fonts.rs::system()`, a scan of every
     installed font) and then opens font files to find fallbacks per script.
   - `mailcraft_engine::cmd::render_pdf(&doc, &fonts, &setup)` — lays the whole message out.
   - `thumbs::pdf_page(&pdf, …)` — rasterises a page at `PREVIEW_PX` = 520 px wide (hayro), **once
     more** if the range doesn't start at page 1.
   `Preview::update` (`print_dialog.rs`) asks for the first preview **immediately** on the first
   frame (`self.shown.is_none()`), so the first frame of the dialog blocks on all of the above.
3. **Printing itself.** `send_print_requests` (`lib.rs` ~line 720) does `print_fonts` +
   `render_pdf` + `lp` inline when the user clicks Print. Same freeze, after the click. Lower
   priority than 1 and 2 but the same fix pattern; do it in the same task.

## The rule

**The dialog is drawn in the first frame after Print, with whatever is already known.** Every
part that needs the system (printer list, each printer's capabilities, fonts, the preview) comes
in later, on its own, and the dialog shows a spinner exactly where that part will appear. The
user can change settings, type a page range, press Escape or *Save as PDF instead* while things
are still loading. Nothing waits on anything else unless it truly must.

## Constraints that shape the design

- `ui-egui` builds for wasm: **no `std::thread`, no `Instant`, no `SystemTime` in it**. Threads
  live in the app (`apps/mailcraft/src/main.rs`) behind the `Services` trait. The precedent is
  `Services::video_frame` (`lib.rs` ~line 178, `main.rs` ~line 250): the UI calls it each frame
  while the answer is `Pending`, the desktop implementation spawns a named thread on the first
  call and parks the result in an `Arc<Mutex<Option<_>>>`. Copy that shape.
- The default `Services` implementations (web, `NullServices` in tests) must answer **at once**
  (no printers / the craft-fonts fallbacks / the preview rendered inline), so headless tests stay
  deterministic and the web app keeps working.
- Results that arrive for a dialog that was closed, or for settings that have since changed, are
  dropped. A background thread never touches UI state; it only fills its slot.
- Time in the UI comes from `ctx.input(|i| i.time)` (already how `Preview` debounces).
- No `unwrap`/`expect`; a poisoned mutex is `unwrap_or_else(PoisonError::into_inner)` as in
  `fonts.rs`.

## Design

### 1. Printers: list first, capabilities after, nothing blocking (net + Services + dialog)

**`crates/net/src/print.rs`**
- Split `list()` into two public steps:
  - `pub fn list_printers() -> Result<Vec<Printer>, String>` — `lpstat -p -d` only; the
    capabilities stay `None`/empty. Keep the "No destinations added" vs scheduler-down handling.
  - `pub fn capabilities(name: &str) -> Option<Printer-caps>` — the `lpoptions -p NAME -l` probe
    for one printer (return the three fields, or a small struct; `None` when the command is
    missing/fails). Keep `valid(name)` in front of it. `MAX_PROBED` stays as a cap on how many
    the app probes.
  - Keep `printers()` as a thin wrapper (list + probe all) for the CLI and existing tests.
- **Timeouts on every child process** in this file: `Command::output()` has none. Add a helper
  `run_with_timeout(cmd, Duration) -> io::Result<Output>` (spawn, poll `try_wait` with short
  sleeps, `kill()` and return a timeout error when over). Use `LIST_TIMEOUT` = 4 s for `lpstat`,
  `PROBE_TIMEOUT` = 3 s per `lpoptions`, `PRINT_TIMEOUT` = 20 s for `lp`. A timeout is an
  ordinary `Err` ("lpstat didn't answer") — never a panic. Test it with a fake script that
  `sleep`s longer than a short test timeout (the fake-script pattern at the bottom of the file;
  never touch a real printer).

**`Services` (`crates/ui-egui/src/lib.rs`)**
- Replace `fn printers(&mut self) -> Result<Vec<Printer>, String>` with a polled shape, e.g.
  ```rust
  pub enum Printers { Pending, Ready(Result<Vec<Printer>, String>) }
  /// The system's printers. Called each frame until it isn't Pending; slow work belongs on another thread.
  fn printers(&mut self) -> Printers { Printers::Ready(Ok(Vec::new())) }
  /// What one printer can do (duplex, colour, papers); Pending while it is being asked.
  fn printer_capabilities(&mut self, name: &str) -> Capabilities /* Pending | Known(…) | Unknown */
  ```
  (Or one call that returns the list plus a per-printer `probing: bool` — pick whichever keeps
  the dialog code simplest, but the list must be shown **before** the capabilities arrive.)
- Desktop (`main.rs`): first call spawns a `print-list` thread that runs `list_printers()` and
  then, in the same thread, `capabilities(name)` for each printer (up to `MAX_PROBED`), publishing
  the list as soon as it's known and each printer's capabilities as they land. Slots are
  `Arc<Mutex<…>>` kept on the `Desktop` struct. Re-opening the dialog later **reuses** the
  answer (printers don't change often) and refreshes it in the background: show the last list
  at once, swap in the new one when it arrives. A `refresh_printers()`/forget on a new
  `Action::Print` is fine; keep it simple.

**`PrintForm` (`crates/ui-egui/src/print_dialog.rs`)**
- `PrintForm::new(printing, saved)` no longer takes the printers. Add a state:
  `printers: Printers`-like (`Loading | Ready(Vec<Printer>) | Failed(String)`), and per-printer
  "capabilities pending" knowledge.
- A new `PrintForm::printers_arrived(&mut self, Result<Vec<Printer>, String>)` does what `new`
  does today **after** the list exists: pick the saved printer by name, else the default, else
  the first, then `fit_to_printer()`. It must **not** reset anything the user has already
  changed (copies, orientation, pages, scale, header). If the user had already opened the combo
  — there's nothing in it yet, so no conflict.
- A `capabilities_arrived(&mut self, name, caps)` updates that printer and re-runs
  `fit_to_printer()` **only if** it is the chosen one.
- `MailApp::ui` polls each frame while the dialog is open and anything is pending (same place
  the video frames are polled, ~line 943) and calls the two `_arrived` methods;
  `ctx.request_repaint_after(100–250 ms)` while pending.
- **Drawing:**
  - *Printer* row while loading: the combo is replaced by `Spinner::new().size(12)` + the
    dimmed note `tl!("Looking for printers…")`. **Print** is disabled until the list is in (it
    can't send anywhere); *Save as PDF instead* works at once.
  - Failed: the existing "No printers were found…" note and the reason, as today.
  - While the **chosen** printer's capabilities are pending: the *Print on both sides*, *Color*
    and *Paper* rows stay **enabled** with every choice offered (unknown = "offer it", as the
    existing rule says), and a 12 px spinner sits at the end of the row with the dimmed note
    `tl!("Asking the printer…")` once (under the grid, not three times). When the capabilities
    land, `fit_to_printer()` greys out what the printer lacks, exactly as now.
  - Everything else (copies, pages, orientation, scale, header, the preview) is drawn and usable
    from the first frame.

### 2. The preview: off the UI thread, debounced as now

- Add to `Services` a generic "do this elsewhere" hook so the UI never spawns threads:
  ```rust
  /// Run `work` off the UI thread when there is one. The default runs it now.
  fn spawn(&mut self, name: &'static str, work: Box<dyn FnOnce() + Send + 'static>) { work() }
  ```
  Desktop: `std::thread::Builder::new().name(name.into()).spawn(work)`; if spawning fails, run
  it inline (never lose the work). This hook is also what step 3 uses.
- **Fonts off-thread too.** `Services::print_fonts(&mut self, text)` can't be called from a
  thread (it borrows `self`). Add
  `fn print_font_finder(&self) -> Arc<dyn Fn(&str) -> Fonts + Send + Sync>`: Desktop returns
  `Arc::new(|t| mailcraft_net::fonts::print_fonts(t))` (`fonts::system()` is already a global
  `Mutex`, so this is thread-safe); the default returns the craft-fonts fallback closure that
  `print_fonts`'s default builds now. Keep `print_fonts` as a wrapper over the finder so nothing
  else changes.
- `MailApp::print_preview(options)` becomes: build a `PreviewJob { key, doc: Doc (cloned),
  options, fonts: Option<Fonts> (the form's cached ones), finder, ui_fonts: (Option<Vec<u8>>,
  Option<Vec<u8>>) }`, a shared `Arc<Mutex<Option<PreviewResult>>>` slot stored on
  `f.preview.in_flight`, and `services.spawn("print-preview", Box::new(move || { … }))` whose
  body is exactly today's closure (fonts if none yet → `render_pdf` → `pdf_page` (twice if
  needed)) ending with `*slot.lock() = Some((key, fonts, result))`. Confirm `Doc`, `Fonts`,
  `PageSetup` and `ColorImage` are `Send` (they're plain data; if something isn't, make it so or
  pass bytes). `render_pdf` and `thumbs::pdf_page` are pure functions and already thread-safe.
- `Preview::update` polls `in_flight` each frame; when the slot is filled it calls the existing
  `finish(key, result)` and stores the fonts on the form (so the second and later previews skip
  the font lookup, as now). While `pending()`: the existing full-size `Spinner` where the page
  will be (first preview) or the small one next to "Page 1 of N" (later ones) — that drawing is
  already right, keep it. `ctx.request_repaint_after(100 ms)` while in flight.
- **One in flight at a time**, as now (`requested`): if the settings change while one is
  rendering, the next starts when the current lands and its key is stale. Don't pile up threads.
- The debounce (`PREVIEW_AFTER` = 0.15 s) stays; the "first one at once" rule stays — "at once"
  now means *started* at once, not *finished*.
- Closing the dialog drops the slot; the thread finishes into an `Arc` nobody reads. Fine.

### 3. Printing itself (`send_print_requests`)

Same hook: for each `PrintRequest`, spawn `"print-send"` doing finder(text) → `render_pdf` →
`print_pdf(options, &pdf)` and park the `Result` plus the printer name in a slot on
`MailApp` (`ui.print_jobs: Vec<Arc<Mutex<Option<…>>>>`). Poll them in `MailApp::ui`; when one
lands, say what today's code says (`Sent to {printer}` / `The message couldn't be printed:
{error}`) via `session.notice`. `print_pdf` moves behind the finder-style `Send` closure too
(`fn print_sender(&self) -> Arc<dyn Fn(&PrintOptions, &[u8]) -> Result<(), String> + Send + Sync>`),
or simply call `mailcraft_net::print::print_pdf` from the thread in the Desktop `spawn` closure —
the point is only that nothing in `ui-egui` names a thread or a net function. Quick print uses
the same path. The headless `Printing` services in `tests.rs` run `spawn` inline, so the
existing assertions (`jobs.borrow().len() == 1` right after the action) still hold.

## Tests (headless, `crates/ui-egui/src/tests.rs`)

Add a test `Services` double, `SlowPrinting`, whose `printers()` answers `Pending` until the
test flips a flag, whose `printer_capabilities` does the same, and whose `spawn` **queues** the
work in a `RefCell<Vec<Box<dyn FnOnce()>>>` that the test drains when it chooses. Then:

- **`the_print_dialog_opens_before_the_system_answers`**: `act(Print)`; after **one** frame the
  dialog is up (`Dialog::Print`), `f.printers` is loading, the combo isn't there, a spinner is,
  Print is disabled, *Save as PDF instead* is enabled, copies/orientation are editable (change
  `f.options.copies`); then release the printer list → next frame the saved printer is chosen
  (same assertions as today's `the_print_dialog_sends_what_was_chosen` opening block) and the
  copies the user set are **kept**; then release capabilities for a `plain_printer()` → duplex
  is `Off`, colour `Monochrome`, paper `A4` (the greying rule), and the "Asking the printer…"
  spinner is gone.
- **`the_preview_is_drawn_off_the_ui_thread`**: with the queue held, three frames show
  `!f.preview.drawn()` and the spinner, and exactly **one** job is queued (debounce + one in
  flight); change the orientation twice while held → still one queued; drain → next frame
  `drawn()` and `total.is_some()`; the stale key triggers one more job for the new orientation
  and only one.
- **`printing_finishes_in_the_background`**: Print with the queue held → dialog closed, no
  notice yet, no job in `jobs`; drain → the `Sent to Label_Maker` notice and one job.
- Keep every existing print test green, adjusting `Dialog::print(...)` call sites (tests.rs
  lines ~203–210 build every dialog for the layout test — the Print ones now take `printing`
  and `saved` only; add a loading variant and a failed variant so both get drawn in light and
  dark).
- `crates/net/src/print.rs`: `list_printers` leaves capabilities unknown; `capabilities` parses
  as before; a fake `lpstat` that `sleep 10`s with a 0.3 s test timeout returns an `Err`
  mentioning the timeout within ~0.5 s and no child is left (check with `try_wait` or just the
  elapsed time).
- `engine/src/tests.rs::every_command_survives_junk_params` stays green (nothing changes there,
  but run it).

## Verify by looking (required)

1. `cargo run -p mailcraft -- --sample --cmd 'list.open={"key":"sample:1:1:INBOX"}' --cmd message.print`
   and screenshot the MailCraft window only: the dialog must be visible in the first screenshot
   you can take.
2. **Simulate a slow CUPS** without touching a printer: a folder with executable fakes
   ```sh
   mkdir -p /tmp/slowcups
   printf '#!/bin/sh\nsleep 4\necho "printer Office_Laser is idle.  enabled since Mon"\necho "system default destination: Office_Laser"\n' > /tmp/slowcups/lpstat
   printf '#!/bin/sh\nsleep 3\necho "Duplex/Double-Sided Printing: *None DuplexNoTumble DuplexTumble"\n' > /tmp/slowcups/lpoptions
   printf '#!/bin/sh\necho "request id is Office_Laser-1 (1 file(s))"\n' > /tmp/slowcups/lp
   chmod +x /tmp/slowcups/*
   PATH=/tmp/slowcups:$PATH cargo run -p mailcraft -- --sample --cmd 'list.open={"key":"sample:1:1:INBOX"}' --cmd message.print
   ```
   Screenshot at ~0.5 s (dialog, printer spinner, preview spinner or page), at ~4.5 s (combo
   filled, "Asking the printer…" spinner), at ~8 s (duplex row enabled, spinner gone). Change
   settings during the wait: it must respond. Press Escape during the wait: it must close and
   the app must not stall afterwards.
3. Time the first frame: with `RUST_LOG=debug`, log `print: dialog open in {ms} ms` from
   `Action::Print` to the end of the first `body()` draw (use `ctx.input(|i| i.time)`, not
   `Instant`, in `ui-egui`); it must be well under 50 ms on the sample mailbox. Remove or keep
   the log at `debug` — don't leave `info` noise.

## i18n

New strings, in **every** `crates/ui-egui/src/i18n/*.tsv` (the tests fail otherwise), short, in
the existing style: `Looking for printers…`, `Asking the printer…`, and the timeout messages if
they are shown (`The print system didn't answer`). Reuse existing strings wherever one fits.

## Docs

- `docs/gaps.md`: under the `v0.19.2 — the Print dialog` entry (or a new `Unreleased` heading
  above it if 0.19.2 is already released — check `ROADMAP.md`), a short paragraph: the dialog
  opens at once; the printers, each printer's capabilities, the fonts and the preview arrive in
  the background with spinners; `lpstat`/`lpoptions`/`lp` have timeouts; printing and the
  preview are rendered off the UI thread.
- `docs/architecture.md`: the `Services` rows for `printers` / `spawn` / `print_font_finder`
  (whatever you add), one line each.
- `ROADMAP.md`: a dated line.

## Order of work (commit after each, `cargo xtask ci` green each time)

1. `net::print`: `list_printers` / `capabilities` split + child-process timeouts + tests.
2. `Services::spawn` + `print_font_finder` + Desktop implementations (no behaviour change yet;
   defaults run inline).
3. Dialog opens immediately: `Printers` polling, `printers_arrived`, `capabilities_arrived`,
   spinners, Print disabled while loading, i18n, the `SlowPrinting` test double and the first
   test.
4. Preview off-thread + its test.
5. Printing off-thread + its test.
6. Look at it (the slow-CUPS run above), docs, `cargo xtask wasm` (the web build sees the new
   `Services` defaults — they must compile there).

## Not in scope

- Any new print option, layout change, or Windows printer selection.
- Caching the font `Collection` to disk (the first scan is slow by nature; it is now off the UI
  thread, which is what matters).
- Cancelling a running `lpstat`/render when the dialog closes beyond letting the thread finish
  into a dropped slot.
