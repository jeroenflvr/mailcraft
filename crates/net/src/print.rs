//! Printing a PDF: the system's printers are listed with CUPS's `lpstat` and printed to with `lp`
//! (Linux, the BSDs and macOS), and on Windows the file goes to the registered PDF app's Print
//! verb, which asks which printer. The system's own commands are run (nothing is linked), with
//! the file in a temporary folder that is cleaned up afterwards.

use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use mailcraft_model::{Printer, Printing};

/// The most printers listed, and copies printed.
const MAX_PRINTERS: usize = 200;
const MAX_COPIES: u32 = 99;
/// Files left in the temporary folder by earlier runs (Windows can't delete one the PDF app is
/// still reading) are removed after this long.
const KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// How printing works here.
pub fn printing() -> Printing {
    if cfg!(windows) { Printing::Viewer } else { Printing::Choose }
}

/// The commands that do it (tests use their own).
struct Tools {
    lpstat: String,
    lp: String,
}

impl Tools {
    fn system() -> Tools {
        Tools { lpstat: "lpstat".into(), lp: "lp".into() }
    }
}

/// The printers the system knows, the default marked. Empty on Windows (the PDF app asks). An
/// error says why none can be listed (no CUPS, its scheduler isn't running).
pub fn printers() -> Result<Vec<Printer>, String> {
    if cfg!(windows) {
        return Ok(Vec::new());
    }
    list(&Tools::system())
}

/// Print a PDF on `printer` (`None`: the system's default), `copies` times. Returns once the print
/// system has taken the job.
pub fn print_pdf(printer: Option<&str>, copies: u32, pdf: &[u8]) -> Result<(), String> {
    send(&Tools::system(), printer, copies, pdf)
}

/// What the commands print in English, whatever the language: the output is read.
fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env("LC_ALL", "C").env("LANG", "C").env("LANGUAGE", "C").stdin(Stdio::null());
    c
}

fn missing(program: &str, e: &std::io::Error) -> String {
    if e.kind() == ErrorKind::NotFound { format!("the print system isn't installed (no `{program}` command)") } else { format!("`{program}`: {e}") }
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
                out.push(Printer { name: name.to_string(), default: false });
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

fn list(tools: &Tools) -> Result<Vec<Printer>, String> {
    let out = command(&tools.lpstat).args(["-p", "-d"]).output().map_err(|e| missing(&tools.lpstat, &e))?;
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

fn send(tools: &Tools, printer: Option<&str>, copies: u32, pdf: &[u8]) -> Result<(), String> {
    if let Some(p) = printer
        && !valid(p)
    {
        return Err(format!("`{}` is not a printer name", p.chars().take(60).collect::<String>()));
    }
    let copies = copies.clamp(1, MAX_COPIES);
    if cfg!(windows) {
        // The PDF app's Print verb; it reads the file after this returns, so the file stays (and is
        // cleaned up by a later run).
        let file = Temp::new(pdf, true)?;
        let one = format!("Start-Process -FilePath {} -Verb Print -WindowStyle Hidden", quoted(&file.path.to_string_lossy()));
        let script = vec![one; copies as usize].join("; ");
        let out = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| missing("powershell.exe", &e))?;
        return if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) };
    }
    let file = Temp::new(pdf, false)?;
    let mut cmd = command(&tools.lp);
    if let Some(p) = printer {
        cmd.arg("-d").arg(p);
    }
    let out =
        cmd.arg("-n").arg(copies.to_string()).arg("-t").arg("MailCraft message").arg(&file.path).output().map_err(|e| missing(&tools.lp, &e))?;
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
        let dir = std::env::temp_dir().join(format!("mailcraft-print-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = |file: &str, body: &str| {
            let path = dir.join(file);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().to_string()
        };
        (Tools { lpstat: script("lpstat", lpstat), lp: script("lp", lp) }, dir)
    }

    const LPSTAT: &str = "printf 'printer Office_Laser is idle.  enabled since Sat 24 Oct 2026 11:42:37 AM CEST\\nprinter Label_Maker disabled since Sat 24 Oct 2026 - Paused\\n\\treason unknown\\nsystem default destination: Office_Laser\\n'";

    #[test]
    fn lists_the_printers_and_the_default() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools("list", LPSTAT, "exit 0");
        let p = list(&t).unwrap();
        assert_eq!(p, vec![Printer { name: "Office_Laser".into(), default: true }, Printer { name: "Label_Maker".into(), default: false }]);
        assert_eq!(parse("printer A is idle.\nprinter A is idle.\nno system default destination\n").len(), 1, "once each, no default");
        assert!(parse("junk\n\n\u{0}printer").iter().all(|p| !p.name.is_empty()));
        // None added is an answer; a scheduler that isn't running is a reason; no CUPS says so.
        let (none, d2) = tools("none", "echo 'lpstat: No destinations added.' >&2; exit 1", "exit 0");
        assert_eq!(list(&none).unwrap(), Vec::new());
        let (down, d3) = tools("down", "echo 'lpstat: Scheduler is not running.' >&2; exit 1", "exit 0");
        assert_eq!(list(&down).unwrap_err(), "Scheduler is not running.");
        let gone = Tools { lpstat: "/nonexistent/lpstat".into(), lp: "/nonexistent/lp".into() };
        assert!(list(&gone).unwrap_err().contains("isn't installed"));
        assert!(send(&gone, None, 1, b"%PDF").unwrap_err().contains("isn't installed"));
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
        send(&t, Some("Office_Laser"), 3, b"%PDF-1.7 test").unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.starts_with("-d Office_Laser -n 3 -t MailCraft message /"), "{args}");
        assert!(args.trim_end().ends_with(".pdf"));
        assert_eq!(std::fs::read(dir.join("got.pdf")).unwrap(), b"%PDF-1.7 test");
        let file = args.trim_end().rsplit(' ').next().unwrap();
        assert!(!std::path::Path::new(file).exists(), "the temporary file is removed");
        // The default printer: no -d; copies are 1 to 99.
        send(&t, None, 0, b"x").unwrap();
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.starts_with("-n 1 -t"), "{args}");
        send(&t, None, 1000, b"x").unwrap();
        assert!(std::fs::read_to_string(dir.join("args")).unwrap().starts_with("-n 99 -t"));
        // A name that could be an option, or isn't one, never reaches lp.
        for bad in ["-o", "", "a b", "a/b", "x\n"] {
            assert!(send(&t, Some(bad), 1, b"x").is_err(), "{bad:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn says_what_lp_said() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let (t, dir) = tools("fail", LPSTAT, "echo 'lp: The printer or class does not exist.' >&2; exit 1");
        assert_eq!(send(&t, Some("Nope"), 1, b"x").unwrap_err(), "The printer or class does not exist.");
        let (t2, d2) = tools("silent", LPSTAT, "exit 2");
        assert!(send(&t2, None, 1, b"x").unwrap_err().contains("exited with"));
        for d in [dir, d2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
