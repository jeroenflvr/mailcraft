//! What the Print dialog chooses: the printer, copies, orientation, paper, scale and pages. Pure
//! data with serde defaults on every field, so older `message.print` params and saved preferences
//! keep working, and every free-text input (the page list) goes through a parser that returns an
//! error rather than panicking.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The most copies one job asks for.
pub const MAX_COPIES: u32 = 99;
/// Scale bounds, in percent.
pub const MIN_SCALE: u16 = 25;
pub const MAX_SCALE: u16 = 400;
/// The longest page list and the most ranges in it that are looked at.
const MAX_PAGES_TEXT: usize = 200;
const MAX_RANGES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Orientation {
    #[default]
    #[serde(alias = "Portrait")]
    Portrait,
    #[serde(alias = "Landscape")]
    Landscape,
}

impl Orientation {
    pub fn code(self) -> &'static str {
        match self {
            Orientation::Portrait => "portrait",
            Orientation::Landscape => "landscape",
        }
    }
}

/// Printing on both sides of the sheet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Duplex {
    #[default]
    Off,
    /// Flip on the long edge (a book).
    #[serde(alias = "long", alias = "longedge", alias = "long-edge")]
    LongEdge,
    /// Flip on the short edge (a notepad).
    #[serde(alias = "short", alias = "shortedge", alias = "short-edge")]
    ShortEdge,
}

impl Duplex {
    pub fn code(self) -> &'static str {
        match self {
            Duplex::Off => "off",
            Duplex::LongEdge => "long",
            Duplex::ShortEdge => "short",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    #[serde(alias = "colour")]
    Color,
    #[serde(alias = "mono", alias = "blackandwhite", alias = "bw")]
    Monochrome,
}

impl ColorMode {
    pub fn code(self) -> &'static str {
        match self {
            ColorMode::Color => "color",
            ColorMode::Monochrome => "monochrome",
        }
    }
}

/// How carefully the printer prints: draft saves ink and time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    #[serde(alias = "fast", alias = "economy")]
    Draft,
    #[default]
    Normal,
    #[serde(alias = "best")]
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Draft, Quality::Normal, Quality::High];

    /// IPP's `print-quality`: 3 draft, 4 normal, 5 high.
    pub fn ipp(self) -> u8 {
        match self {
            Quality::Draft => 3,
            Quality::Normal => 4,
            Quality::High => 5,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Paper {
    #[default]
    #[serde(alias = "a4")]
    A4,
    #[serde(alias = "letter", alias = "Letter")]
    Letter,
    #[serde(alias = "legal", alias = "Legal")]
    Legal,
    #[serde(alias = "a3")]
    A3,
    #[serde(alias = "a5")]
    A5,
}

impl Paper {
    pub const ALL: [Paper; 5] = [Paper::A4, Paper::Letter, Paper::Legal, Paper::A3, Paper::A5];

    /// The name CUPS (`media=`) and the dialog use.
    pub fn code(self) -> &'static str {
        match self {
            Paper::A4 => "A4",
            Paper::Letter => "Letter",
            Paper::Legal => "Legal",
            Paper::A3 => "A3",
            Paper::A5 => "A5",
        }
    }

    pub fn from_code(s: &str) -> Option<Paper> {
        Paper::ALL.into_iter().find(|p| p.code().eq_ignore_ascii_case(s.trim()))
    }
}

/// How big the content is on the paper.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scale {
    /// As wide as the page (the default).
    #[default]
    FitToPage,
    /// A fixed percentage, `MIN_SCALE..=MAX_SCALE`.
    Percent(u16),
}

impl Scale {
    /// The percentage the content is laid out at: fit to page is 100 %.
    pub fn percent(self) -> u16 {
        match self {
            Scale::FitToPage => 100,
            Scale::Percent(p) => p.clamp(MIN_SCALE, MAX_SCALE),
        }
    }

    pub fn parse(s: &str) -> Result<Scale, String> {
        let s = s.trim().trim_end_matches('%').trim();
        if s.is_empty() || s.eq_ignore_ascii_case("fit") || s.eq_ignore_ascii_case("fittopage") {
            return Ok(Scale::FitToPage);
        }
        let p: u16 = s.parse().map_err(|_| format!("'{s}' isn't a percentage"))?;
        if !(MIN_SCALE..=MAX_SCALE).contains(&p) {
            return Err(format!("the scale is between {MIN_SCALE}% and {MAX_SCALE}%"));
        }
        Ok(Scale::Percent(p))
    }
}

impl fmt::Display for Scale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scale::FitToPage => f.write_str("fit"),
            Scale::Percent(p) => write!(f, "{p}"),
        }
    }
}

impl Serialize for Scale {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Scale::FitToPage => s.serialize_str("fit"),
            Scale::Percent(p) => s.serialize_u16(*p),
        }
    }
}

impl<'de> Deserialize<'de> for Scale {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Scale, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Number(u64),
        }
        match Raw::deserialize(d)? {
            Raw::Text(t) => Scale::parse(&t).map_err(serde::de::Error::custom),
            Raw::Number(n) => Scale::parse(&n.to_string()).map_err(serde::de::Error::custom),
        }
    }
}

/// Which pages are printed: all, or the listed ranges (1-based, sorted, merged).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PageRange {
    #[default]
    All,
    Range(Vec<(u32, u32)>),
}

impl PageRange {
    /// Parse "1-3, 5": ranges and single pages, separated by commas (or semicolons or spaces).
    /// An empty text is all pages. Page 0, a range that runs backwards and anything that isn't a
    /// number are errors. The ranges come back sorted and merged.
    pub fn parse(text: &str) -> Result<PageRange, String> {
        if text.len() > MAX_PAGES_TEXT {
            return Err("the page list is too long".into());
        }
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for part in text.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
            if ranges.len() >= MAX_RANGES {
                return Err("too many page ranges".into());
            }
            let number = |s: &str| -> Result<u32, String> {
                let s = s.trim();
                match s.parse::<u32>() {
                    Ok(0) => Err("pages start at 1".to_string()),
                    Ok(n) => Ok(n),
                    Err(_) => Err(format!("'{s}' isn't a page number")),
                }
            };
            let (a, b) = match part.split_once('-') {
                Some((a, b)) => (number(a)?, number(b)?),
                None => {
                    let n = number(part)?;
                    (n, n)
                }
            };
            if a > b {
                return Err(format!("'{part}' runs backwards"));
            }
            ranges.push((a, b));
        }
        if ranges.is_empty() {
            return Ok(PageRange::All);
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        Ok(PageRange::Range(merged))
    }

    pub fn is_all(&self) -> bool {
        matches!(self, PageRange::All)
    }

    pub fn contains(&self, page: u32) -> bool {
        match self {
            PageRange::All => true,
            PageRange::Range(r) => r.iter().any(|&(a, b)| (a..=b).contains(&page)),
        }
    }

    /// The pages of a `total`-page document that are printed, in order (never more than `total`).
    pub fn pages(&self, total: u32) -> Vec<u32> {
        match self {
            PageRange::All => (1..=total).collect(),
            PageRange::Range(r) => r.iter().flat_map(|&(a, b)| a.max(1)..=b.min(total)).collect(),
        }
    }
}

impl fmt::Display for PageRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let PageRange::Range(r) = self {
            for (i, &(a, b)) in r.iter().enumerate() {
                if i > 0 {
                    f.write_str(",")?;
                }
                if a == b {
                    write!(f, "{a}")?;
                } else {
                    write!(f, "{a}-{b}")?;
                }
            }
        }
        Ok(())
    }
}

impl Serialize for PageRange {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PageRange {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<PageRange, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Number(u32),
        }
        let text = match Raw::deserialize(d)? {
            Raw::Text(t) => t,
            Raw::Number(n) => n.to_string(),
        };
        PageRange::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Everything the Print dialog chooses. The defaults are what it opens with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrintOptions {
    /// `None`: the system's default printer.
    pub printer: Option<String>,
    /// 1 to 99.
    pub copies: u32,
    pub collate: bool,
    pub orientation: Orientation,
    pub duplex: Duplex,
    pub color: ColorMode,
    /// Normal leaves the printer's own setting.
    pub quality: Quality,
    pub paper: Paper,
    pub scale: Scale,
    pub pages: PageRange,
    /// The subject / From / To / Date block above the body.
    pub headers: bool,
}

impl Default for PrintOptions {
    fn default() -> Self {
        PrintOptions {
            printer: None,
            copies: 1,
            collate: true,
            orientation: Orientation::Portrait,
            duplex: Duplex::Off,
            color: ColorMode::Color,
            quality: Quality::Normal,
            paper: Paper::A4,
            scale: Scale::FitToPage,
            pages: PageRange::All,
            headers: true,
        }
    }
}

impl PrintOptions {
    /// The options with every number brought into its range and an empty printer name dropped.
    pub fn normalized(mut self) -> PrintOptions {
        self.copies = self.copies.clamp(1, MAX_COPIES);
        if let Scale::Percent(p) = self.scale {
            self.scale = Scale::Percent(p.clamp(MIN_SCALE, MAX_SCALE));
        }
        if self.printer.as_deref().is_some_and(|p| p.trim().is_empty()) {
            self.printer = None;
        }
        self
    }

    /// What is remembered between runs: everything except the page list.
    pub fn remembered(&self) -> PrintOptions {
        PrintOptions { pages: PageRange::All, ..self.clone() }.normalized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_page_lists() {
        assert_eq!(PageRange::parse("").unwrap(), PageRange::All);
        assert_eq!(PageRange::parse("  ").unwrap(), PageRange::All);
        assert_eq!(PageRange::parse("1-3, 5").unwrap(), PageRange::Range(vec![(1, 3), (5, 5)]));
        assert_eq!(PageRange::parse("5,1-3").unwrap(), PageRange::Range(vec![(1, 3), (5, 5)]));
        assert_eq!(PageRange::parse("1-3,4,6-7;7").unwrap(), PageRange::Range(vec![(1, 4), (6, 7)]));
        assert_eq!(PageRange::parse("2-2").unwrap(), PageRange::Range(vec![(2, 2)]));
        assert_eq!(PageRange::parse("1 - 3").unwrap().to_string(), "1-3");
        assert_eq!(PageRange::parse("5,1-3,,").unwrap().to_string(), "1-3,5");
    }

    #[test]
    fn rejects_junk_page_lists() {
        for bad in ["0", "3-1", "a", "1-", "-3", "1--3", "1-2-3", "1.5", "-1", "99999999999", "1,x", "४"] {
            assert!(PageRange::parse(bad).is_err(), "{bad:?} should be an error");
        }
        assert!(PageRange::parse(&"1,".repeat(300)).is_err());
        assert!(PageRange::parse(&(1..=100).map(|n| format!("{},", n * 2)).collect::<String>()).is_err());
    }

    #[test]
    fn selects_pages() {
        let r = PageRange::parse("2-3,5,9").unwrap();
        assert_eq!(r.pages(5), vec![2, 3, 5]);
        assert!(r.pages(1).is_empty());
        assert!(r.contains(9) && !r.contains(4));
        assert_eq!(PageRange::Range(vec![(0, 2)]).pages(5), vec![1, 2]);
        assert_eq!(PageRange::All.pages(3), vec![1, 2, 3]);
        assert_eq!(PageRange::parse("1-4000000000").unwrap().pages(2), vec![1, 2]);
    }

    #[test]
    fn scales() {
        assert_eq!(Scale::parse("fit").unwrap(), Scale::FitToPage);
        assert_eq!(Scale::parse("75%").unwrap(), Scale::Percent(75));
        assert!(Scale::parse("10").is_err() && Scale::parse("401").is_err() && Scale::parse("x").is_err());
        assert_eq!(Scale::Percent(0).percent(), MIN_SCALE);
        assert_eq!(Scale::FitToPage.percent(), 100);
    }

    #[test]
    fn old_params_still_work() {
        let o: PrintOptions = serde_json::from_str(r#"{"printer":"Office","copies":3}"#).unwrap();
        assert_eq!(o.printer.as_deref(), Some("Office"));
        assert_eq!(o.copies, 3);
        assert!(o.collate && o.headers);
        assert_eq!(o.paper, Paper::A4);
        assert_eq!(serde_json::from_str::<PrintOptions>("{}").unwrap(), PrintOptions::default());
    }

    #[test]
    fn full_options_round_trip() {
        let o: PrintOptions = serde_json::from_str(
            r#"{"printer":"P","copies":2,"collate":false,"orientation":"landscape","duplex":"long","color":"mono","quality":"draft",
                "paper":"letter","scale":75,"pages":"1-3, 5","headers":false}"#,
        )
        .unwrap();
        assert_eq!(o.orientation, Orientation::Landscape);
        assert_eq!(o.duplex, Duplex::LongEdge);
        assert_eq!(o.color, ColorMode::Monochrome);
        assert_eq!(o.quality, Quality::Draft);
        assert_eq!(o.paper, Paper::Letter);
        assert_eq!(o.scale, Scale::Percent(75));
        assert_eq!(o.pages, PageRange::Range(vec![(1, 3), (5, 5)]));
        assert!(!o.headers && !o.collate);
        let back: PrintOptions = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(back, o);
        let fit: PrintOptions = serde_json::from_str(&serde_json::to_string(&PrintOptions::default()).unwrap()).unwrap();
        assert_eq!(fit, PrintOptions::default());
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        for bad in [
            r#"{"copies":-1}"#,
            r#"{"scale":"x"}"#,
            r#"{"pages":"3-1"}"#,
            r#"{"paper":"B7"}"#,
            r#"{"quality":"best-ever"}"#,
            r#"{"copies":"many"}"#,
            "7",
        ] {
            assert!(serde_json::from_str::<PrintOptions>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn normalizes_and_remembers() {
        let o = PrintOptions {
            copies: 500,
            scale: Scale::Percent(9000),
            printer: Some(" ".into()),
            pages: PageRange::Range(vec![(2, 3)]),
            ..Default::default()
        };
        let n = o.clone().normalized();
        assert_eq!((n.copies, n.scale, n.printer), (MAX_COPIES, Scale::Percent(MAX_SCALE), None));
        assert_eq!(PrintOptions { copies: 0, ..Default::default() }.normalized().copies, 1);
        assert_eq!(o.remembered().pages, PageRange::All);
    }
}
