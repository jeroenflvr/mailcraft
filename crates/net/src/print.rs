//! Printing a PDF: the system's printers are listed with CUPS's `lpstat` and printed to with `lp`
//! (Linux, the BSDs and macOS), and on Windows the file goes to the registered PDF app's Print
//! verb, which asks which printer. The system's own commands are run (nothing is linked), with
//! the file in a temporary folder that is cleaned up afterwards.

use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use mailcraft_model::{Capabilities, ColorMode, Duplex, Paper, PrintOptions, Printer, Printing, Quality};

/// The most printers listed, and copies printed.
const MAX_PRINTERS: usize = 200;
const MAX_COPIES: u32 = mailcraft_model::printing::MAX_COPIES;
/// Printers asked about their capabilities (each asks CUPS once).
pub const MAX_PROBED: usize = 50;
/// How long each command may take before it is given up on: a remote or sleepy CUPS, or a network
/// printer that is off, must never leave a caller waiting for ever.
const LIST_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const PRINT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most a command's output is read (it is hostile like anything else).
const MAX_OUTPUT: u64 = 1 << 20;
/// Files left in the temporary folder by earlier runs (Windows can't delete one the PDF app is
/// still reading) are removed after this long.
const KEEP: Duration = Duration::from_secs(24 * 3600);

/// How printing works here.
pub fn printing() -> Printing {
    if cfg!(windows) { Printing::Viewer } else { Printing::Choose }
}

/// The commands that do it (tests use their own).
struct Tools {
    lpstat: String,
    lp: String,
    lpoptions: String,
    list_timeout: Duration,
    probe_timeout: Duration,
    print_timeout: Duration,
}

impl Tools {
    fn system() -> Tools {
        Tools {
            lpstat: "lpstat".into(),
            lp: "lp".into(),
            lpoptions: "lpoptions".into(),
            list_timeout: LIST_TIMEOUT,
            probe_timeout: PROBE_TIMEOUT,
            print_timeout: PRINT_TIMEOUT,
        }
    }
}

/// The printers the system knows, the default marked, with what each can do. Empty on Windows (the
/// PDF app asks). An error says why none can be listed (no CUPS, its scheduler isn't running).
/// Slow with many printers: an app with a screen lists them with `list_printers` and asks
/// `capabilities` of each afterwards.
pub fn printers() -> Result<Vec<Printer>, String> {
    if cfg!(windows) {
        return Ok(Vec::new());
    }
    list(&Tools::system())
}

/// The printers the system knows, the default marked, one quick question to the print system
/// (`lpstat`). What each can do is left unknown: `capabilities` asks that.
pub fn list_printers() -> Result<Vec<Printer>, String> {
    if cfg!(windows) {
        return Ok(Vec::new());
    }
    list_only(&Tools::system())
}

/// What one printer can do. `None` when it can't be asked (no `lpoptions`, it didn't answer in
/// time, the name isn't one CUPS has), which leaves everything offered.
pub fn capabilities(name: &str) -> Option<Capabilities> {
    if cfg!(windows) {
        return None;
    }
    probe(&Tools::system(), name)
}

/// Print a PDF as `options` say (the printer, copies, collating, both sides, colour, paper).
/// Orientation, scale and the page range are in the PDF itself. Returns once the print system has
/// taken the job. On Windows the PDF app's Print verb can't take options, so only the copies apply.
pub fn print_pdf(options: &PrintOptions, pdf: &[u8]) -> Result<(), String> {
    send(&Tools::system(), options, pdf)
}

/// What the commands print in English, whatever the language: the output is read.
fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env("LC_ALL", "C").env("LANG", "C").env("LANGUAGE", "C").stdin(Stdio::null());
    c
}

/// Run a command and collect its output, killing it when it takes longer than `timeout`
/// (`ErrorKind::TimedOut`). The output is read on other threads so a full pipe can't stall the
/// child, and is capped.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Output> {
    fn reader<R: Read + Send + 'static>(from: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("print-read".into()).spawn(move || {
            let mut bytes = Vec::new();
            if let Some(from) = from {
                let _ = from.take(MAX_OUTPUT).read_to_end(&mut bytes);
            }
            let _ = tx.send(bytes);
        });
        if spawned.is_err() {
            return std::sync::mpsc::channel().1;
        }
        rx
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (out, err) = (reader(child.stdout.take()), reader(child.stderr.take()));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            // The readers are left to end by themselves: something the command started may hold its pipes open.
            return Err(std::io::Error::new(ErrorKind::TimedOut, "the command didn't answer in time"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // The command is done; anything it left running must not keep this waiting.
    let grace = Duration::from_millis(500);
    Ok(Output { status, stdout: out.recv_timeout(grace).unwrap_or_default(), stderr: err.recv_timeout(grace).unwrap_or_default() })
}

fn missing(program: &str, e: &std::io::Error) -> String {
    match e.kind() {
        ErrorKind::NotFound => format!("the print system isn't installed (no `{program}` command)"),
        ErrorKind::TimedOut => format!("the print system didn't answer (`{program}`)"),
        _ => format!("`{program}`: {e}"),
    }
}

/// `lpstat -p -d` as printers.
fn parse(text: &str) -> Vec<Printer> {
    let mut out: Vec<Printer> = Vec::new();
    let mut default = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("printer ") {
            if let Some(name) = rest.split_whitespace().next()
                && out.len() < MAX_PRINTERS
                && !out.iter().any(|p| p.name == name)
            {
                out.push(Printer { name: name.to_string(), ..Printer::default() });
            }
        } else if let Some(d) = line.strip_prefix("system default destination:") {
            default = Some(d.trim().to_string());
        }
    }
    for p in &mut out {
        p.default = default.as_deref() == Some(p.name.as_str());
    }
    out
}

/// What `lpoptions -p NAME -l` says a printer can do. Lines look like
/// `Duplex/2-Sided Printing: *None DuplexNoTumble DuplexTumble`, the current choice starred. A
/// feature the output doesn't mention stays unknown (`None`), which the dialog treats as offered.
fn parse_capabilities(text: &str, into: &mut Printer) {
    for line in text.lines().take(500) {
        let Some((head, values)) = line.split_once(':') else { continue };
        let key = head.split('/').next().unwrap_or("").trim();
        let values: Vec<String> = values.split_whitespace().take(200).map(|v| v.trim_start_matches('*').to_ascii_lowercase()).collect();
        match key {
            "Duplex" => into.duplex = Some(values.iter().any(|v| !matches!(v.as_str(), "none" | "off" | "false"))),
            "ColorModel" | "print-color-mode" | "ColorMode" | "OutputMode" => {
                let gray = |v: &str| {
                    matches!(v, "gray" | "grey" | "grayscale" | "mono" | "monochrome" | "black" | "blackwhite" | "bw" | "fastgray" | "highgray")
                };
                into.color = Some(values.iter().any(|v| !gray(v)));
            }
            "PageSize" => {
                let mut papers: Vec<Paper> = Vec::new();
                for p in values.iter().filter_map(|v| Paper::from_code(v)) {
                    if !papers.contains(&p) {
                        papers.push(p);
                    }
                }
                into.papers = papers;
            }
            _ => {}
        }
    }
}

/// `lpstat -p -d`, as printers with nothing known about what they can do.
fn list_only(tools: &Tools) -> Result<Vec<Printer>, String> {
    let out = run_with_timeout(command(&tools.lpstat).args(["-p", "-d"]), tools.list_timeout).map_err(|e| missing(&tools.lpstat, &e))?;
    let printers = parse(&String::from_utf8_lossy(&out.stdout));
    if printers.is_empty() && !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        // No printer added is an answer; a scheduler that isn't there is a reason.
        if !why.contains("No destinations added") {
            return Err(why.trim().trim_start_matches("lpstat: ").to_string());
        }
    }
    Ok(printers)
}

/// `lpoptions -p NAME -l`. A missing or failing `lpoptions` just leaves the capabilities unknown.
fn probe(tools: &Tools, name: &str) -> Option<Capabilities> {
    if !valid(name) {
        return None;
    }
    let out = run_with_timeout(command(&tools.lpoptions).arg("-p").arg(name).arg("-l"), tools.probe_timeout).ok()?;
    if !out.status.success() {
        return None;
    }
    let mut p = Printer::default();
    parse_capabilities(&String::from_utf8_lossy(&out.stdout), &mut p);
    Some(Capabilities { duplex: p.duplex, color: p.color, papers: p.papers })
}

fn list(tools: &Tools) -> Result<Vec<Printer>, String> {
    let mut printers = list_only(tools)?;
    for p in printers.iter_mut().take(MAX_PROBED) {
        if let Some(c) = probe(tools, &p.name) {
            p.learn(c);
        }
    }
    Ok(printers)
}

/// A temporary file that is removed when it goes out of scope (unless kept for the PDF app).
struct Temp {
    path: PathBuf,
    keep: bool,
}

impl Temp {
    fn new(bytes: &[u8], keep: bool) -> Result<Temp, String> {
        let dir = std::env::temp_dir();
        remove_old(&dir);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let path = dir.join(format!("mailcraft-print-{}-{nanos}.pdf", std::process::id()));
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Temp { path, keep })
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn remove_old(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > KEEP);
        if name.starts_with("mailcraft-print-") && name.ends_with(".pdf") && old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A printer name `lp` can be given safely (no options, no spaces: CUPS names have none).
fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.starts_with('-') && !name.chars().any(|c| c.is_control() || c.is_whitespace() || c == '/')
}

/// PowerShell's single-quoted string.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The `lp -o` options for a job. Every value comes from a closed set, so nothing typed by the
/// user reaches `lp` but the printer name, which `valid` has checked. Orientation, scale and pages
/// are in the PDF (it is laid out that way), and the PDF's size is the paper's, so none is passed.
fn job_options(o: &PrintOptions, copies: u32) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut option = |v: String| {
        args.push("-o".to_string());
        args.push(v);
    };
    if copies > 1 {
        option(format!("collate={}", if o.collate { "true" } else { "false" }));
    }
    option(
        match o.duplex {
            Duplex::Off => "sides=one-sided",
            Duplex::LongEdge => "sides=two-sided-long-edge",
            Duplex::ShortEdge => "sides=two-sided-short-edge",
        }
        .to_string(),
    );
    option(
        match o.color {
            ColorMode::Color => "print-color-mode=color",
            ColorMode::Monochrome => "print-color-mode=monochrome",
        }
        .to_string(),
    );
    // Normal is the printer's own setting, left alone.
    if o.quality != Quality::Normal {
        option(format!("print-quality={}", o.quality.ipp()));
    }
    option(format!("media={}", o.paper.code()));
    args
}

fn send(tools: &Tools, options: &PrintOptions, pdf: &[u8]) -> Result<(), String> {
    let printer = options.printer.as_deref();
    if let Some(p) = printer
        && !valid(p)
    {
        return Err(format!("`{}` is not a printer name", p.chars().take(60).collect::<String>()));
    }
    let copies = options.copies.clamp(1, MAX_COPIES);
    if cfg!(windows) {
        // The PDF app's Print verb; it reads the file after this returns, so the file stays (and is
        // cleaned up by a later run).
        let file = Temp::new(pdf, true)?;
        let one = format!("Start-Process -FilePath {} -Verb Print -WindowStyle Hidden", quoted(&file.path.to_string_lossy()));
        let script = vec![one; copies as usize].join("; ");
        let out = run_with_timeout(
            Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).stdin(Stdio::null()),
            tools.print_timeout,
        )
        .map_err(|e| missing("powershell.exe", &e))?;
        return if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) };
    }
    let file = Temp::new(pdf, false)?;
    let mut cmd = command(&tools.lp);
    if let Some(p) = printer {
        cmd.arg("-d").arg(p);
    }
    cmd.arg("-n").arg(copies.to_string()).arg("-t").arg("MailCraft message").args(job_options(options, copies));
    let out = run_with_timeout(cmd.arg(&file.path), tools.print_timeout).map_err(|e| missing(&tools.lp, &e))?;
    if out.status.success() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim().trim_start_matches("lp: ");
        Err(if why.is_empty() { format!("`lp` exited with {}", out.status) } else { why.to_string() })
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use super::*;

    /// One test at a time: a script that is still open for writing can't be run (`ETXTBSY`) when
    /// another test's process starts at that moment.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// Fake `lpstat` and `lp` scripts in a folder of their own: no real printer is ever asked.
    fn tools(name: &str, lpstat: &str, lp: &str) -> (Tools, PathBuf) {
        tools_with(name, lpstat, lp, "exit 1")
    }

    fn tools_with(name: &str, lpstat: &str, lp: &str, lpoptions: &str) -> (Tools, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mailcraft-print-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = |file: &str, body: &str| {
            let path = dir.join(file);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().to_string()
        };
        let system = Tools::system();
        (Tools { lpstat: script("lpstat", lpstat), lp: script("lp", lp), lpoptions: script("lpoptions", lpoptions), ..system }, dir)
    }

    const LPSTAT: &str = "printf 'printer Office_Laser is idle.  enabled since Sat 24 Oct 2026 11:42:37 AM CEST\\nprinter Label_Maker disabled since Sat 24 Oct 2026 - Paused\\n\\treason unknown\\nsystem default destination: Office_Laser\\n'";

    #[test]
    fn lists_the_printers_and_the_default() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools("list", LPSTAT, "exit 0");
        let p = list(&t).unwrap();
        assert_eq!(
            p,
            vec![
                Printer { name: "Office_Laser".into(), default: true, ..Printer::default() },
                Printer { name: "Label_Maker".into(), ..Printer::default() }
            ]
        );
        assert_eq!(parse("printer A is idle.\nprinter A is idle.\nno system default destination\n").len(), 1, "once each, no default");
        assert!(parse("junk\n\n\u{0}printer").iter().all(|p| !p.name.is_empty()));
        // None added is an answer; a scheduler that isn't running is a reason; no CUPS says so.
        let (none, d2) = tools("none", "echo 'lpstat: No destinations added.' >&2; exit 1", "exit 0");
        assert_eq!(list(&none).unwrap(), Vec::new());
        let (down, d3) = tools("down", "echo 'lpstat: Scheduler is not running.' >&2; exit 1", "exit 0");
        assert_eq!(list(&down).unwrap_err(), "Scheduler is not running.");
        let gone = Tools {
            lpstat: "/nonexistent/lpstat".into(),
            lp: "/nonexistent/lp".into(),
            lpoptions: "/nonexistent/lpoptions".into(),
            ..Tools::system()
        };
        assert!(list(&gone).unwrap_err().contains("isn't installed"));
        assert!(send(&gone, &PrintOptions::default(), b"%PDF").unwrap_err().contains("isn't installed"));
        for d in [dir, d2, d3] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// `lp` gets the printer, the copies and the file, which is there while it runs and gone after.
    #[test]
    fn hands_the_pdf_to_lp_and_cleans_up() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) =
            tools("lp", LPSTAT, "printf '%s ' \"$@\" > \"$(dirname \"$0\")/args\"; for last; do :; done; cp \"$last\" \"$(dirname \"$0\")/got.pdf\"");
        let to = |printer: Option<&str>, copies: u32| PrintOptions { printer: printer.map(str::to_string), copies, ..PrintOptions::default() };
        send(&t, &to(Some("Office_Laser"), 3), b"%PDF-1.7 test").unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.starts_with("-d Office_Laser -n 3 -t MailCraft message -o collate=true -o sides=one-sided "), "{args}");
        assert!(args.trim_end().ends_with(".pdf"));
        assert_eq!(std::fs::read(dir.join("got.pdf")).unwrap(), b"%PDF-1.7 test");
        let file = args.trim_end().rsplit(' ').next().unwrap();
        assert!(!std::path::Path::new(file).exists(), "the temporary file is removed");
        // The default printer: no -d; copies are 1 to 99.
        send(&t, &to(None, 0), b"x").unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.starts_with("-n 1 -t MailCraft message -o sides=one-sided"), "{args}");
        assert!(!args.contains("collate"), "one copy has nothing to collate: {args}");
        send(&t, &to(None, 1000), b"x").unwrap();
        assert!(std::fs::read_to_string(dir.join("args")).unwrap().starts_with("-n 99 -t"));
        // A name that could be an option, or isn't one, never reaches lp.
        for bad in ["-o", "", "a b", "a/b", "x\n"] {
            assert!(send(&t, &to(Some(bad), 1), b"x").is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn says_what_lp_said() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools("fail", LPSTAT, "echo 'lp: The printer or class does not exist.' >&2; exit 1");
        assert_eq!(
            send(&t, &PrintOptions { printer: Some("Nope".into()), ..PrintOptions::default() }, b"x").unwrap_err(),
            "The printer or class does not exist."
        );
        let (t2, d2) = tools("silent", LPSTAT, "exit 2");
        assert!(send(&t2, &PrintOptions::default(), b"x").unwrap_err().contains("exited with"));
        for d in [dir, d2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    fn args_of(o: &PrintOptions) -> String {
        job_options(o, o.copies).join(" ")
    }

    /// The exact `-o` list for a few option sets.
    #[test]
    fn maps_options_to_lp() {
        assert_eq!(args_of(&PrintOptions::default()), "-o sides=one-sided -o print-color-mode=color -o media=A4");
        let full = PrintOptions {
            copies: 2,
            collate: false,
            duplex: Duplex::LongEdge,
            color: ColorMode::Monochrome,
            paper: Paper::Letter,
            orientation: mailcraft_model::Orientation::Landscape,
            pages: mailcraft_model::PageRange::Range(vec![(2, 3)]),
            scale: mailcraft_model::Scale::Percent(50),
            ..PrintOptions::default()
        };
        assert_eq!(args_of(&full), "-o collate=false -o sides=two-sided-long-edge -o print-color-mode=monochrome -o media=Letter");
        let draft = PrintOptions { quality: Quality::Draft, ..Default::default() };
        assert_eq!(args_of(&draft), "-o sides=one-sided -o print-color-mode=color -o print-quality=3 -o media=A4");
        let high = PrintOptions { quality: Quality::High, ..Default::default() };
        assert!(args_of(&high).contains("-o print-quality=5 "), "{}", args_of(&high));
        let short = PrintOptions { copies: 3, duplex: Duplex::ShortEdge, paper: Paper::A5, ..PrintOptions::default() };
        assert_eq!(args_of(&short), "-o collate=true -o sides=two-sided-short-edge -o print-color-mode=color -o media=A5");
    }

    /// The options reach `lp` in order, and never as anything but those from the closed sets.
    #[test]
    fn lp_gets_the_options() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools("opts", LPSTAT, "printf '%s ' \"$@\" > \"$(dirname \"$0\")/args\"");
        let o = PrintOptions {
            printer: Some("Office_Laser".into()),
            copies: 2,
            duplex: Duplex::ShortEdge,
            color: ColorMode::Monochrome,
            paper: Paper::Legal,
            ..PrintOptions::default()
        };
        send(&t, &o, b"x").unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(
            args.starts_with("-d Office_Laser -n 2 -t MailCraft message -o collate=true -o sides=two-sided-short-edge -o print-color-mode=monochrome -o media=Legal /"),
            "{args}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    const LPOPTIONS: &str = "PageSize/Media Size: Letter *A4 Legal A4.Borderless Tabloid\\nDuplex/2-Sided Printing: *None DuplexNoTumble DuplexTumble\\nColorModel/Color Model: Gray *RGB\\nResolution/Resolution: *600dpi\\n";

    #[test]
    fn reads_what_a_printer_can_do() {
        let mut p = Printer { name: "P".into(), ..Printer::default() };
        parse_capabilities(&LPOPTIONS.replace("\\n", "\n"), &mut p);
        assert_eq!((p.duplex, p.color), (Some(true), Some(true)));
        assert_eq!(p.papers, vec![Paper::Letter, Paper::A4, Paper::Legal]);

        let mut mono = Printer::default();
        parse_capabilities("Duplex/Two-Sided: *None\nColorModel/Output: *Gray\nPageSize/Paper: *A4\n", &mut mono);
        assert_eq!((mono.duplex, mono.color, mono.papers.clone()), (Some(false), Some(false), vec![Paper::A4]));

        // Nothing said, junk, and huge lines: unknown, not an error.
        let mut none = Printer::default();
        parse_capabilities("", &mut none);
        parse_capabilities("junk\n:::\nDuplex\n\u{0}/:\n", &mut none);
        assert_eq!((none.duplex, none.color, none.papers.is_empty()), (None, None, true));
        let mut big = Printer::default();
        parse_capabilities(&format!("PageSize/x: {}\n", "A4 ".repeat(100_000)), &mut big);
        assert_eq!(big.papers, vec![Paper::A4]);
    }

    #[test]
    fn lists_capabilities_with_the_printers() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools_with("caps", LPSTAT, "exit 0", &format!("if [ \"$2\" = Office_Laser ]; then printf '{LPOPTIONS}'; else exit 1; fi"));
        let p = list(&t).unwrap();
        assert_eq!((p[0].duplex, p[0].color), (Some(true), Some(true)));
        assert_eq!(p[0].papers.len(), 3);
        assert_eq!((p[1].duplex, p[1].color, p[1].papers.len()), (None, None, 0), "a failing lpoptions leaves them unknown");
        // No lpoptions at all: the printers are still listed.
        let gone = Tools { lpoptions: "/nonexistent/lpoptions".into(), ..tools("nolpopt", LPSTAT, "exit 0").0 };
        assert_eq!(list(&gone).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The list alone leaves what a printer can do unknown, and one printer is asked on its own.
    #[test]
    fn lists_first_and_asks_each_printer_after() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools_with("split", LPSTAT, "exit 0", &format!("printf '{LPOPTIONS}'"));
        let p = list_only(&t).unwrap();
        assert_eq!(p.len(), 2);
        assert!(p.iter().all(|p| (p.duplex, p.color, p.papers.is_empty()) == (None, None, true)));
        let c = probe(&t, "Office_Laser").unwrap();
        assert_eq!((c.duplex, c.color, c.papers.len()), (Some(true), Some(true), 3));
        assert_eq!(probe(&t, "-o"), None, "a name that isn't safe is never passed on");
        let (broken, d2) = tools_with("split-fail", LPSTAT, "exit 0", "exit 1");
        assert_eq!(probe(&broken, "Office_Laser"), None);
        for d in [dir, d2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// A command that doesn't answer is killed and is an ordinary error, quickly.
    #[test]
    fn gives_up_on_a_command_that_does_not_answer() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools_with("slow", "exec sleep 10", "exec sleep 10", "exec sleep 10");
        let t = Tools {
            list_timeout: Duration::from_millis(300),
            probe_timeout: Duration::from_millis(300),
            print_timeout: Duration::from_millis(300),
            ..t
        };
        let start = Instant::now();
        let e = list_only(&t).unwrap_err();
        assert!(e.contains("didn't answer"), "{e}");
        assert_eq!(probe(&t, "Office_Laser"), None);
        let e = send(&t, &PrintOptions::default(), b"x").unwrap_err();
        assert!(e.contains("didn't answer"), "{e}");
        assert!(start.elapsed() < Duration::from_millis(1500), "{:?}", start.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Output bigger than a pipe holds doesn't stall the command.
    #[test]
    fn reads_a_lot_of_output() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let out = run_with_timeout(Command::new("sh").args(["-c", "yes | head -c 300000"]), Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout.len(), 300_000);
    }
}
