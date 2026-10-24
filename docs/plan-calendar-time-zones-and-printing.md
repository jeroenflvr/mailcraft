# Action plan: calendar times an hour off, and printing

Two problems reported on 2026-10-24 (Debian 13, Europe/Brussels), with their root causes as found
in the code and what to change. Each arc below is sized for one agent session; land each with its
regression tests and `cargo xtask ci` green, and commit after every arc.

## Problem 1 — own appointments shown an hour late (9–13 instead of 8–12)

### Evidence

- Today is 2026-10-24; European daylight time ends tomorrow, 2026-10-25 (last Sunday of October).
  Anything looked at for next week lies on the other side of a UTC-offset change.
- Invitations from other people (one-off events, received by mail, times in UTC) show correctly;
  the user's own appointments (typically recurring series, or events created on another device
  with a `TZID`) are an hour late. Both patterns point at time zone handling, not at the clock.

### Root causes (all four are real; any one of them produces the symptom)

**A. One UTC offset for all time.** [`Clock`](../crates/model/src/dates.rs) holds a single
`offset: i32`, the offset *now*, read each frame from `chrono::Local::now()`
([apps/mailcraft/src/main.rs](../apps/mailcraft/src/main.rs) `Services::clock`). Every local
conversion uses it, whatever the instant being converted:

| Site | What it does with `clock.offset` |
|---|---|
| `model/src/dates.rs` `civil()`, `local_day()` | all displayed times (also message dates) |
| `engine/src/session/pim.rs` `cal_day`, `local_midnight`, `agenda` | the calendar grid's day boundaries, all-day placement |
| `engine/src/cmd.rs` `event_from` (~l.1621) and ~l.543 | `YYYY-MM-DDTHH:MM` from commands → UTC |
| `ui-egui/src/calendar.rs` l.24, l.35, `local_midnight` calls | day/week/month grids, "now" line |
| `ui-egui/src/pim_dialogs.rs` l.100 and `local_time`/`local_date` | the event editor |
| `ui-egui/src/reading.rs` l.375, l.1765 | invitation times, delayed send |
| `model/src/organize.rs` l.187–189 | "tomorrow morning" follow-ups |
| `engine/src/session/invite.rs` l.53 | floating times in received `.ics` |

An event on Monday 2026-10-26 at 08:00 CET is 07:00Z; shown with today's +02:00 it reads 09:00.
Everything next week is an hour late until the clock itself crosses the change (and then
everything *this* week is an hour early when scrolled back to).

**B. Recurrences expand in UTC.** [`ical::starts`](../crates/model/src/ical.rs) takes
`time = e.start.rem_euclid(86_400)` — the UTC time of day of the first occurrence — and repeats it
on every later day. A weekly 08:00 series created in winter is stored as 07:00Z; its summer
occurrences are still 07:00Z = 09:00 CEST. A series created in summer shows an hour *early* in
winter. RFC 5545 says occurrences repeat at the same *local* time in the event's zone. This
affects every source: CalDAV (`TZID` lost at parse, see C), Graph and EWS
([`graph_pim.rs`](../crates/net/src/graph_pim.rs) fetches with `Prefer: outlook.timezone="UTC"`
and ignores `recurrence.range.recurrenceTimeZone` / `originalStartTimeZone`), and events created
in MailCraft.

**C. `TZID` is dropped and CalDAV parses with offset 0.** `CalEvent` has no zone field; `parse`
converts to UTC and forgets the zone. [`dav.rs`](../crates/net/src/dav.rs) l.535 calls
`ical::parse(&data, 0)`: an event whose `TZID` names a zone the file has no `VTIMEZONE` for (Olson
names like `Europe/Brussels` are often sent bare) is read as UTC, two hours off in summer, one in
winter. Floating times get the same treatment.

**D. Writing back loses the zone.** `ical::write_event` always writes `DTSTART:…Z`; `graph_pim`
`event_json` always sends `"timeZone": "UTC"`; EWS likewise. Editing a recurring series in
MailCraft (even just its title) rewrites it as a UTC-floating series, so *other* devices start
showing it an hour off after the next DST change. This is the most damaging one: MailCraft
corrupts the user's calendar.

### Design

Keep the engine pure and wasm-safe (no tz database I/O in `model`; the browser has no file
system). Two building blocks:

1. **A zone as a transition list** — `model::dates::Zone { id: String, transitions: Vec<(i64 /*utc*/, i32 /*offset after*/)>, default: i32 }`
   with `offset_at(utc)` (binary search, saturating), `local_to_utc(local_secs)` (resolve with
   the offset in force just before; for a gap pick the later offset, for an overlap the earlier —
   document it), and a yearly-rule constructor so the existing `VTIMEZONE` parser produces one
   (`Zone::from_rules(standard, daylight)` expanding rules for, say, 1970–2100 lazily or on a
   bounded range around the instant). Pure integer maths like the rest of `dates.rs`.
2. **The local zone comes from the platform** — extend `Services::clock()` to return the local
   `Zone` too (or add `Services::local_zone() -> Zone`, called at start and when the system zone
   changes). Native: build it from `chrono::Local` by probing offsets at, e.g., each day over
   ±5 years from now and recording the days where the offset changes, then bisecting to the
   hour inside each changed day (no new dependency; `chrono` is already in the app). Web: the same
   with `js_sys::Date::get_timezone_offset`. Also carry the IANA name when the platform gives it
   (`TZ`, `/etc/localtime` symlink target, `Intl.DateTimeFormat().resolvedOptions().timeZone`),
   for `TZID` output.
   `Clock` becomes `{ now, zone: Zone }` with `offset_at(t)`; keep a `Clock::fixed(now, offset)`
   constructor so the many tests that use `Clock::new(NOW, 0)` keep working unchanged.

Then:

- **`CalEvent` gets a zone.** Add `#[serde(default)] pub tz: Option<String>` (the `TZID` or IANA
  name as written) and `#[serde(default, skip_serializing_if = "Option::is_none")] pub zone: Option<Zone>`
  (the `VTIMEZONE` rules, when the file had them, so they can be written back verbatim or as
  rules). Keep `start`/`end` in UTC seconds as today so the rest of the code is untouched. The
  on-disk `pim.json` stays readable (both fields default).
- **`ical::parse(text, local: &Zone)`** instead of `local_offset: i64`. `TZID` without a
  `VTIMEZONE`: look it up in the local zone when the names match, else fall back to a small
  built-in table of the common Olson and Windows names → rules (Europe, US, UK, Australia; the
  CLDR `windowsZones` mapping for the Windows names is permissively licensed and tiny once limited
  to rules), else the local zone *with a warning event*, never UTC. Floating times use the local
  zone.
- **`ical::starts(e, before, local: &Zone)`** expands in the event's zone (`e.zone`, else its
  `tz` through the table, else `local`): take the first occurrence's local wall time, repeat it
  on each candidate day, convert each back with `local_to_utc`. `UNTIL` is already UTC; `EXDATE`
  and `RECURRENCE-ID` comparisons stay in UTC. `occurrences()` and `Session::agenda` pass the
  local zone through.
- **Writing preserves the zone.** `ical::write_event`: when `e.tz`/`e.zone` is set and the event
  recurs (or always, for fidelity), write `DTSTART;TZID=<tz>:<local>` and a `VTIMEZONE` block from
  the rules (`write()` collects the zones used). One-off events may stay UTC. New events made in
  MailCraft get `tz = local zone's IANA name` when known. Graph: send
  `"timeZone": "<IANA name>"` with local wall times (Graph accepts IANA names) and read
  `recurrence.range.recurrenceTimeZone` / `originalStartTimeZone` (Windows names → table) into
  `tz`. EWS: `StartTimeZone`/`MeetingTimeZone` the same way.
- **Everything in the table above** uses `clock.offset_at(t)` / `clock.zone.local_to_utc(..)`
  instead of `clock.offset`. `Session::local_midnight(day)` resolves through the zone;
  `calendar.rs` draws the "now" line with `offset_at(now)`; the event editor converts the typed
  local date-time with `local_to_utc`; `parse_local` takes a `&Zone`.
- **CLI/web clocks** (`apps/mailcraft-cli/src/main.rs` l.70, `apps/mailcraft-web/src/web.rs`
  l.132) build the zone the same way; `sample.rs` seeding keeps a fixed offset.

### Steps

1. `model/src/dates.rs`: `Zone`, `Clock { now, zone }`, `offset_at`, `local_to_utc`,
   `Clock::fixed`. Tests: Brussels 2026 transitions (2026-03-29 01:00Z → +7200, 2026-10-25 01:00Z
   → +3600); `civil()` on both sides of the change; the gap hour and the repeated hour.
2. `model/src/ical.rs`: `Zone` built from `VTIMEZONE` rules (replace `ZonePart` lookup with the
   shared type); `CalEvent.tz`/`zone`; `parse` takes `&Zone`; the built-in name table; `starts`
   expands in the event's zone; `write_event` writes `TZID` + `VTIMEZONE`. Tests: a weekly 08:00
   Europe/Brussels series starting 2026-01-12 has its 2026-07-06 occurrence at 06:00Z and its
   2026-11-02 one at 07:00Z; `TZID=Europe/Brussels` without `VTIMEZONE` resolves (not UTC); parse
   → write → parse round-trips the zone; `every_command_survives_junk_params` stays green.
3. `net/src/dav.rs`: parse with the account's local zone (pass it in `sync(...)`; the engine has
   it). `graph_pim.rs` / `ews_pim.rs`: read and write zones. Tests against `fake_dav.rs` /
   `fake_graph.rs`: a series pushed and read back keeps its wall time across a DST change.
4. Engine and UI call sites (table above) → `offset_at`; `Services::clock` returns the zone;
   `set_clock(now, zone)`. Native builder from `chrono::Local`; web builder from `js_sys::Date`.
   Headless frame test in `ui-egui/src/tests.rs`: week view spanning 2026-10-25 with an 08:00 CET
   event on the Monday draws it in the 08:00 row.
5. `docs/sync.md` (CalDAV/Graph sections) and `docs/gaps.md`: note zones are kept and recurrences
   expand in the event's zone; `ROADMAP.md` entry.

### Acceptance

- Week of 2026-10-26 viewed on 2026-10-24: an event stored at 07:00Z (08:00 CET) shows 08:00.
- A weekly series created in January at 08:00 shows 08:00 in July and in November.
- After editing a recurring series in MailCraft, the server copy still has a `TZID`/`timeZone`
  and other clients show it unchanged.
- `cargo xtask wasm` still builds (no clock or file reads in portable code).

## Problem 2 — only "print to PDF", and squares in the printed text

### Evidence

- `Action::Print` ([ui-egui/src/lib.rs](../crates/ui-egui/src/lib.rs) ~l.1267) asks where to save
  a PDF, writes it with `message.exportPdf`, opens it with the system viewer and says "print it
  from the viewer". There is no path to a printer.
- [`mailcraft-print`](../crates/print/src/lib.rs) embeds exactly one font (plus an optional bold)
  — on Linux the first of Noto Sans / DejaVu Sans found at three hard-coded paths
  (`apps/mailcraft/src/main.rs` `system_fonts()`). `Face::encode` maps a character the font lacks
  to glyph 0 (`.notdef`), which is the square. Checked against this machine's
  `NotoSans-Regular.ttf`: it has **no** box drawing (`─ │ ┼ ═ ━`, which `mime::html_to_rich`
  emits for every HTML table), arrows and check marks (`→ ✓ ✔ ★ ▾ ☐`), emoji, the emoji variation
  selector U+FE0F (so every emoji is two squares), math (`≥ ∞`), CJK, Hebrew or Arabic. A Noto
  install also has no `NotoSans-Bold.ttf` at that path on some distributions, so bold falls back
  to Helvetica with `?` for anything outside Windows-1252.
- Fonts are also not found at all on distributions with other layouts (NixOS, Fedora's
  `/usr/share/fonts/google-noto`), giving `?` instead of squares; macOS picks `Arial.ttf`, which
  lacks the same symbols; Windows picks Segoe UI, which has arrows but no emoji or CJK.

### Design

**Fonts.** Use the system's font collection for printing exactly as the UI already does for
drawing: [`fontique`](https://crates.io/crates/fontique) (Apache-2.0 OR MIT, already compiled into
the app through eframe's `system_fonts` feature) enumerates installed fonts with their coverage and
has `fallback_families(script)`. Keep `mailcraft-print` pure and I/O-free: it receives bytes and
chooses per character.

- `print::Fonts` becomes `{ regular, bold, fallbacks: Vec<&[u8]> /* (data, face index) */ }`.
- The layout splits each line into **runs by face**: for each char, the first of regular →
  fallbacks whose `cmap` has it. Zero-width and format characters (U+200B–U+200F, U+2028/9,
  U+2060, U+FE00–FE0F, U+FEFF, U+00AD) are dropped before shaping, never drawn. A character no
  face has is drawn as `?` from the regular face (never glyph 0) — and a note in the log.
- Collections (`.ttc`) need a face index: `Face::parse(data, index)`. CFF-outline fonts (`.otf`,
  including Noto Sans CJK) can't go in `FontFile2`; either embed them as `FontFile3`/`OpenType`
  (PDF 1.6, ISO 32000-1 §9.9: `/Subtype /OpenType`, pass the whole file) or skip them as
  fallbacks. Implement `FontFile3` — it is a few lines and CJK on Linux is almost always CFF.
- Each used face becomes one `/F<n>` CID font with its own `/W` and `/ToUnicode`; the content
  stream switches fonts within a line. Subsetting is not required (the files are compressed), but
  cap the total embedded size (e.g. 40 MB) and drop the largest fallbacks beyond it with a log line.
- Width measurement for wrapping goes through the same run selection.
- In the app, `Services` gains `print_fonts(text: &str) -> Vec<(Vec<u8>, u32)>`: from a lazily
  built `fontique::Collection`, the UI font first (whatever `install_fonts` chose), then for each
  script present in `text` the first fallback family that has a usable face; dedupe by file. The
  engine's `print_fonts` field goes away in favour of asking the services at print time (the
  engine stays sans-I/O: the UI collects the bytes and passes them into `message_pdf`). Web:
  the craft-fonts already embedded (`ui-egui/src/craft_fonts.rs`) serve as fallbacks.
- Drop the hard-coded paths in `system_fonts()` in favour of `fontique` for the UI font too, with
  the current paths as last resort (fixes NixOS/Fedora).

**Printing.** Add a real print action beside Save as PDF, platform by platform, no `unsafe`:

- Linux/BSD (CUPS): `lpstat -p -d` lists printers and the default; `lp -d <printer> -n <copies> <file.pdf>`
  prints (PDF is CUPS's native format). No daemon → tell the user and offer the PDF route.
- macOS: the same `lp`/`lpstat` (CUPS ships with macOS).
- Windows: `powershell -NoProfile -Command Start-Process -FilePath <pdf> -Verb Print -WindowStyle Hidden`
  hands the file to the registered PDF handler's print verb (Edge, Acrobat); printer choice stays
  with that app. Document the limitation; a native dialog needs the `windows` crate and `unsafe`,
  which the workspace forbids.
- `Services` gains `printers() -> Vec<Printer { name, default }>` and
  `print_pdf(printer: Option<&str>, copies: u32, pdf: &[u8]) -> Result<(), String>` (writes the
  PDF to the app's temp folder, spawns the command, deletes the file after the job is handed
  over). Web: `window.print()` of the PDF is not possible from wasm; keep "Save as PDF".
- UI: **Print** opens a small `Dialog::Print` (printer list with the default selected, copies,
  "Open as PDF instead" link); the ribbon's "Quick print" prints to the default printer at once,
  as Outlook's does. Commands: `message.print` with `{"printer": "...", "copies": n}` — note it is
  a command that performs I/O in the app layer via `Services`, like `message.exportPdf` writes a
  file; the engine itself doesn't spawn processes.
- Interface strings through `tl!`/`tf!` and every catalog in `ui-egui/src/i18n/`.

### Steps

1. `crates/print`: runs by face, fallbacks, zero-width filtering, `?` not glyph 0, `.ttc` index,
   `FontFile3` for CFF, per-face objects, size cap. Tests: a doc with `→ ✓ 😀 ─ 中` and Noto Sans
   as regular + DejaVu Sans / a CJK font as fallbacks (skip when not installed) contains no
   `<0000>` CID in its content streams and has one `/ToUnicode` per face; a doc with U+FE0F draws
   nothing for it; `empty_and_hostile_input` extended with a truncated `.ttc` and a CFF font as
   regular.
2. `apps/mailcraft`: `fontique::Collection` behind a `OnceLock`; `Services::print_fonts`;
   `system_fonts()` through fontique with the old paths as fallback; `cjk_font`/`rtl_font` can
   use it too (separate arc if large).
3. `ui-egui`: `message_pdf` takes the fonts; `Dialog::Print`; `Action::Print` → dialog,
   `Action::QuickPrint` → default printer; "Save as PDF" unchanged. Headless frame test for the
   dialog in `ui-egui/src/tests.rs`.
4. `apps/mailcraft`: `printers()`/`print_pdf()` for Linux/macOS (`lpstat`, `lp`) and Windows
   (`Start-Process -Verb Print`). `mailcraft-cli print [--printer X]` for scripting.
5. `docs/gaps.md`, `docs/architecture.md` (print row: "fallback fonts, CUPS/shell printing"),
   `ROADMAP.md`; `ATTRIBUTION.md` unchanged (no new assets).

### Acceptance

- Printing an HTML mail with a table, an arrow and an emoji produces a PDF with no squares and
  the text copy-able from the viewer (`ToUnicode` correct for every face).
- On Debian with CUPS and a printer configured, **Print** lists the printer and the job appears
  in `lpstat -o`; without CUPS the dialog explains and offers the PDF.
- `cargo xtask ci`, `cargo xtask layers` (no process spawning below the app layer) and
  `cargo xtask wasm` pass.

## Order

Problem 1 first: it shows wrong data and, through root cause D, writes wrong data to the server.
Within it, steps 1–2 (model) can be reviewed on their own before the call sites change. Problem 2
step 1 (fonts) is independent of everything else and can run in parallel.
