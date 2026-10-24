//! The system's fonts, found by name and by script with `fontique` (the font collection the
//! interface's toolkit uses too): for the interface, and for printing, where each message gets the
//! regular and bold faces and the fallback fonts its characters need (arrows, check marks, box
//! drawing, other scripts), so nothing prints as a box.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use fontique::{Collection, CollectionOptions, FallbackKey, FamilyInfo, FontStyle, FontWeight, FontWidth, GenericFamily, Script};
use mailcraft_print::{FontFile, Fonts, covers};

/// The interface's font families, best first.
const UI_FAMILIES: &[&str] = if cfg!(target_os = "windows") {
    &["Segoe UI", "Arial"]
} else if cfg!(target_os = "macos") {
    &["Helvetica Neue", "Arial", "Helvetica"]
} else {
    &["Noto Sans", "DejaVu Sans", "Liberation Sans", "Cantarell", "Ubuntu"]
};

/// Families with symbols, arrows, box drawing and emoji outlines, tried for what no script's
/// fallback has.
const SYMBOL_FAMILIES: &[&str] = &[
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "Noto Sans Math",
    "DejaVu Sans",
    "Segoe UI Symbol",
    "Segoe UI Emoji",
    "Apple Symbols",
    "Arial Unicode MS",
    "Noto Emoji",
    "Symbola",
    "FreeSans",
];

/// Most fallback fonts handed over for one message.
const MAX_FALLBACKS: usize = 8;
/// Fonts larger than this aren't kept in memory between prints.
const KEEP_BYTES: usize = 8 << 20;

struct System {
    collection: Collection,
    /// Loaded faces by (family, bold).
    files: HashMap<(String, bool), Option<FontFile>>,
}

fn system() -> &'static Mutex<System> {
    static SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();
    SYSTEM.get_or_init(|| {
        Mutex::new(System { collection: Collection::new(CollectionOptions { shared: false, system_fonts: true }), files: HashMap::new() })
    })
}

/// A family's regular or bold face, read from its file.
fn load(info: &FamilyInfo, bold: bool) -> Option<FontFile> {
    let weight = if bold { FontWeight::BOLD } else { FontWeight::NORMAL };
    let font = info.match_font(FontWidth::NORMAL, FontStyle::Normal, weight, false)?;
    // A family without a bold face answers with its regular one.
    if bold && font.weight().value() < 600.0 {
        return None;
    }
    let blob = font.load(None)?;
    Some(FontFile::new(blob.data().to_vec(), font.index()))
}

impl System {
    fn family_file(&mut self, name: &str, bold: bool) -> Option<FontFile> {
        if let Some(hit) = self.files.get(&(name.to_string(), bold)) {
            return hit.clone();
        }
        let file = self.collection.family_by_name(name).and_then(|info| load(&info, bold));
        if file.as_ref().is_none_or(|f| f.data.len() <= KEEP_BYTES) {
            self.files.insert((name.to_string(), bold), file.clone());
        }
        file
    }

    /// The interface's regular and bold faces.
    fn ui_pair(&mut self) -> (Option<FontFile>, Option<FontFile>) {
        let generic: Vec<String> = {
            let ids: Vec<_> = self.collection.generic_families(GenericFamily::SansSerif).collect();
            ids.into_iter().filter_map(|id| self.collection.family_name(id).map(str::to_string)).collect()
        };
        for name in UI_FAMILIES.iter().map(|n| n.to_string()).chain(generic) {
            if let Some(regular) = self.family_file(&name, false) {
                let bold = self.family_file(&name, true);
                return (Some(regular), bold);
            }
        }
        (None, None)
    }

    /// The families the system recommends for a script.
    fn script_families(&mut self, script: [u8; 4]) -> Vec<String> {
        let ids: Vec<_> = self.collection.fallback_families(FallbackKey::new(Script::from_bytes(script), None)).collect();
        ids.into_iter().filter_map(|id| self.collection.family_name(id).map(str::to_string)).collect()
    }
}

/// The script (ISO 15924) of a character that has one of its own, for asking the system for a
/// font; symbols and the rest are looked for in [`SYMBOL_FAMILIES`].
fn script_of(c: char) -> Option<[u8; 4]> {
    Some(*match u32::from(c) {
        0x0370..=0x03FF | 0x1F00..=0x1FFF => b"Grek",
        0x0400..=0x052F => b"Cyrl",
        0x0530..=0x058F => b"Armn",
        0x0590..=0x05FF | 0xFB1D..=0xFB4F => b"Hebr",
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF => b"Arab",
        0x0900..=0x097F => b"Deva",
        0x0980..=0x09FF => b"Beng",
        0x0E00..=0x0E7F => b"Thai",
        0x10A0..=0x10FF => b"Geor",
        0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => b"Hang",
        0x3040..=0x309F => b"Hira",
        0x30A0..=0x30FF | 0x31F0..=0x31FF => b"Kana",
        0x2E80..=0x2FDF | 0x3000..=0x303F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FFFF => b"Hani",
        _ => return None,
    })
}

/// The interface's regular and bold faces as plain font files (a face of a collection can't be
/// handed to the toolkit by its bytes alone, so one is only used when it is the first).
pub fn ui_fonts() -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let (regular, bold) = system().lock().unwrap_or_else(std::sync::PoisonError::into_inner).ui_pair();
    let plain = |f: Option<FontFile>| f.filter(|f| f.index == 0).map(|f| f.data.to_vec());
    (plain(regular), plain(bold))
}

/// The fonts to print `text` with: the interface's regular and bold, then fallbacks for the
/// characters they lack, from the system's recommendation for each script and then a few known
/// families of symbols.
pub fn print_fonts(text: &str) -> Fonts {
    let mut sys = system().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (regular, bold) = sys.ui_pair();
    let mut fonts = Fonts { regular, bold, fallbacks: Vec::new() };
    let mut missing = fonts.missing(text);
    let mut tried: Vec<String> = Vec::new();
    let mut take = |sys: &mut System, fonts: &mut Fonts, missing: &mut Vec<char>, name: &str| {
        if missing.is_empty() || fonts.fallbacks.len() >= MAX_FALLBACKS || tried.iter().any(|t| t == name) {
            return;
        }
        tried.push(name.to_string());
        let Some(file) = sys.family_file(name, false) else { return };
        if missing.iter().any(|c| covers(&file, *c)) {
            missing.retain(|c| !covers(&file, *c));
            fonts.fallbacks.push(file);
        }
    };
    // Each script in the text: what the system recommends for it.
    let mut scripts: Vec<[u8; 4]> = Vec::new();
    for c in &missing {
        if let Some(s) = script_of(*c)
            && !scripts.contains(&s)
        {
            scripts.push(s);
        }
    }
    for script in scripts {
        for name in sys.script_families(script) {
            take(&mut sys, &mut fonts, &mut missing, &name);
        }
    }
    for name in SYMBOL_FAMILIES {
        take(&mut sys, &mut fonts, &mut missing, name);
    }
    if !missing.is_empty() {
        log::info!("print: no installed font has {} of the characters", missing.len());
    }
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_of_characters() {
        assert_eq!(script_of('中'), Some(*b"Hani"));
        assert_eq!(script_of('я'), Some(*b"Cyrl"));
        assert_eq!(script_of('م'), Some(*b"Arab"));
        assert_eq!(script_of('→'), None);
        assert_eq!(script_of('a'), None);
    }

    /// With the system's fonts: a message with arrows, a check mark, box drawing and Chinese is
    /// drawn from fonts that have them (skipped where no font is installed at all).
    #[test]
    fn finds_fallbacks_for_symbols_and_scripts() {
        let text = "Plan → done ✓ ┼─│ ═ 中文 Привет";
        let fonts = print_fonts(text);
        let Some(regular) = &fonts.regular else { return };
        assert!(covers(regular, 'a'));
        let missing = fonts.missing(text);
        // Whatever this machine has: what it could find is used, and nothing is asked twice.
        assert!(fonts.fallbacks.len() <= MAX_FALLBACKS);
        assert!(!missing.contains(&'a') && missing.iter().all(|c| !fonts.fallbacks.iter().any(|f| covers(f, *c))));
        if std::path::Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf").exists() {
            assert!(!missing.contains(&'→') && !missing.contains(&'┼'), "{missing:?}");
        }
        let (r, _) = ui_fonts();
        assert!(r.is_none_or(|r| r.len() > 1000));
    }
}
