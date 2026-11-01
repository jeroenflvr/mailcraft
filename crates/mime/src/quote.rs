//! The original under a reply or forward as it was received, to send on as Outlook does: its
//! HTML, cleaned of what mustn't travel (scripts, event handlers, `javascript:` links, forms,
//! frames, comments, the document's head), with its pictures carried along as `cid:` parts.
//!
//! The HTML is read with html5ever's tokenizer (as the layout is) and written out again, so what
//! comes out is well-formed tags and escaped text whatever went in. Pictures on the web stay the
//! sender's addresses (forwarding them is what Outlook does; showing them here still waits for
//! the user to ask), and `data:` pictures stay as they are.

use std::cell::{Cell, RefCell};

use html5ever::TokenizerResult;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};
use mail_parser::MessageParser;
use serde::{Deserialize, Serialize};

use crate::{HTML_MAX, InlinePicture, Parsed, html_body, layout, picture_attachment};

/// Most pictures carried with a quoted original, and their bytes together.
const MAX_PICTURES: usize = 50;
const MAX_PICTURE_BYTES: usize = 15 << 20;

/// The original under a reply or forward, as it was received.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quoted {
    /// The original's HTML body, cleaned, its pictures pointing at the message's own `cid:`s.
    pub html: String,
    /// Who sent it to whom and when (From, Sent, To, Cc, Subject), as HTML.
    pub header_html: String,
    /// The quoted part of the text as it was put into the message (the header lines and the
    /// original), to tell whether it was changed.
    pub text: String,
}

/// An original's HTML, ready to quote: the HTML, the pictures it now refers to, which of the
/// message's attachments those are, and each picture address the original used with its `cid`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuotedHtml {
    pub html: String,
    pub pictures: Vec<InlinePicture>,
    pub carried: Vec<usize>,
    pub map: Vec<(String, String)>,
}

/// The HTML body of `raw` (parsed as `p`), cleaned to quote, its pictures from the message carried
/// under fresh `cid:`s from `cid_for`; its pictures from the web under the `cid:`s `web` gives
/// them (`None`: left as web addresses). `None` for a message without an HTML body.
pub fn quoted_html(raw: &[u8], p: &Parsed, cid_for: &mut dyn FnMut() -> String, web: &mut dyn FnMut(&str) -> Option<String>) -> Option<QuotedHtml> {
    let html = layout::inline_css(&html_body(raw)?);
    let m = MessageParser::default().parse(raw)?;
    let parts: Vec<_> = m.attachments().collect();
    let mut q = QuotedHtml::default();
    let mut bytes = 0usize;
    let mut rewrite = |src: &str| -> Option<String> {
        if let Some((_, cid)) = q.map.iter().find(|(s, _)| s == src) {
            return Some(format!("cid:{cid}"));
        }
        let lower = src.trim().to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            let cid = web(src.trim())?;
            q.map.push((src.to_string(), cid.clone()));
            return Some(format!("cid:{cid}"));
        }
        let index = picture_attachment(p, src)?;
        let info = p.attachments.iter().find(|a| a.index == index)?;
        let cid = match q.carried.iter().position(|i| *i == index) {
            Some(n) => q.pictures.get(n)?.cid.clone(),
            None => {
                let data = parts.get(index)?.contents();
                if !info.mime.starts_with("image/") || q.pictures.len() >= MAX_PICTURES || bytes.saturating_add(data.len()) > MAX_PICTURE_BYTES {
                    return None;
                }
                bytes += data.len();
                let cid = cid_for();
                q.pictures.push(InlinePicture { cid: cid.clone(), name: info.name.clone(), mime: info.mime.clone(), data: data.to_vec() });
                q.carried.push(index);
                cid
            }
        };
        q.map.push((src.to_string(), cid.clone()));
        Some(format!("cid:{cid}"))
    };
    let cleaned = clean_html(&html, &mut rewrite);
    q.html = cleaned;
    Some(q)
}

/// `html` without what mustn't be sent on, its picture addresses (`<img src>`, `background`,
/// CSS `url()`) changed where `rewrite` says. The `<body>`'s content when there is one (as a
/// `<div>` keeping the body's look), else all of it. Never panics; empty when reading fails.
pub fn clean_html(html: &str, rewrite: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let capped = match html.char_indices().nth(HTML_MAX) {
        Some((i, _)) => html.get(..i).unwrap_or(html),
        None => html,
    };
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sink = Clean {
            out: RefCell::new(String::with_capacity(capped.len())),
            skip: RefCell::new(None),
            body: Cell::new(false),
            ended: Cell::new(false),
            rewrite: RefCell::new(rewrite),
        };
        let tok = Tokenizer::new(sink, TokenizerOpts::default());
        let queue = BufferQueue::default();
        queue.push_back(StrTendril::from(capped));
        while let TokenizerResult::Script(()) = tok.feed(&queue) {}
        tok.end();
        tok.sink.out.take()
    }));
    done.unwrap_or_default()
}

/// Left out with everything inside them (a self-closing slash doesn't end them in HTML).
const DROPPED: [&str; 10] = ["script", "style", "title", "iframe", "object", "applet", "noembed", "noframes", "template", "textarea"];
/// Left out, their content kept (a head left open still has the body after it).
const UNWRAPPED: [&str; 15] =
    ["html", "head", "meta", "link", "base", "embed", "form", "input", "button", "select", "option", "noscript", "param", "frameset", "frame"];
/// Elements without content (no end tag).
const VOID: [&str; 14] = ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
/// Attributes holding an address.
const URL_ATTRS: [&str; 12] =
    ["href", "src", "action", "formaction", "background", "poster", "data", "xlink:href", "lowsrc", "dynsrc", "longdesc", "cite"];

struct Clean<'a> {
    out: RefCell<String>,
    /// The element being left out and how deep inside it (the same element nested).
    skip: RefCell<Option<(String, usize)>>,
    /// A `<body>` was seen (what came before it is dropped).
    body: Cell<bool>,
    /// The `</body>` was seen, or the output is full: nothing more.
    ended: Cell<bool>,
    rewrite: RefCell<&'a mut dyn FnMut(&str) -> Option<String>>,
}

fn esc_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\0' => {}
            c => out.push(c),
        }
    }
}

fn esc_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\0' => {}
            c => out.push(c),
        }
    }
}

/// An address that runs something (`javascript:`, `vbscript:`) or is a document of its own
/// (`data:` other than a picture): browsers ignore whitespace and controls inside the scheme.
fn unsafe_url(v: &str) -> bool {
    let squeezed: String = v.chars().filter(|c| !c.is_whitespace() && !c.is_control()).take(32).collect::<String>().to_ascii_lowercase();
    squeezed.starts_with("javascript:")
        || squeezed.starts_with("vbscript:")
        || (squeezed.starts_with("data:") && !squeezed.starts_with("data:image/"))
}

/// CSS that runs something (old Internet Explorer and Firefox extensions).
fn unsafe_css(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    ["expression(", "javascript:", "vbscript:", "behavior:", "-moz-binding", "@import"].iter().any(|b| l.contains(b))
}

/// `css` with each `url(…)` changed where `rewrite` says.
fn rewrite_urls(css: &str, rewrite: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let lower = css.to_ascii_lowercase();
    let mut out = String::with_capacity(css.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(at) = lower.get(from..).and_then(|r| r.find("url(")).map(|i| i + from) {
        let open = at + 4;
        let Some(close) = css.get(open..).and_then(|r| r.find(')')).map(|i| i + open) else { break };
        let inner = css.get(open..close).unwrap_or("");
        let src = inner.trim().trim_matches(['"', '\'']).trim();
        if let Some(new) = rewrite(src) {
            out.push_str(css.get(last..open).unwrap_or(""));
            out.push('\'');
            out.push_str(&new.replace(['\'', '"', ')', '\\'], ""));
            out.push('\'');
            last = close;
        }
        from = close + 1;
    }
    out.push_str(css.get(last..).unwrap_or(""));
    out
}

impl Clean<'_> {
    fn attrs(&self, tag: &str, attrs: &[html5ever::Attribute], out: &mut String) {
        let mut rewrite = self.rewrite.borrow_mut();
        for a in attrs.iter().take(64) {
            let name = a.name.local.to_string().to_ascii_lowercase();
            let mut value = a.value.to_string();
            if name.starts_with("on") || name == "srcset" || name.contains(['"', '\'', '<', '>', '/', '=']) || name.is_empty() {
                continue;
            }
            if URL_ATTRS.contains(&name.as_str()) && unsafe_url(&value) {
                continue;
            }
            if name == "style" {
                if unsafe_css(&value) {
                    continue;
                }
                value = rewrite_urls(&value, &mut **rewrite);
            }
            if ((name == "src" && tag == "img") || name == "background")
                && let Some(new) = rewrite(value.trim())
            {
                value = new;
            }
            out.push(' ');
            out.push_str(&name);
            out.push_str("=\"");
            esc_attr(&value, out);
            out.push('"');
        }
    }
}

impl TokenSink for Clean<'_> {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        if self.ended.get() {
            return TokenSinkResult::Continue;
        }
        let raw = |name: &str| match name {
            "script" => TokenSinkResult::RawData(RawKind::ScriptData),
            "textarea" | "title" => TokenSinkResult::RawData(RawKind::Rcdata),
            "style" | "iframe" | "noembed" | "noframes" | "xmp" => TokenSinkResult::RawData(RawKind::Rawtext),
            _ => TokenSinkResult::Continue,
        };
        let mut skip = self.skip.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                let name = tag.name.to_string().to_ascii_lowercase();
                if let Some((dropped, depth)) = skip.as_mut() {
                    if *dropped == name {
                        match tag.kind {
                            TagKind::StartTag => *depth += 1,
                            TagKind::EndTag => *depth = depth.saturating_sub(1),
                        }
                        if *depth == 0 {
                            *skip = None;
                        }
                    }
                    return TokenSinkResult::Continue;
                }
                let mut out = self.out.borrow_mut();
                match tag.kind {
                    TagKind::StartTag => {
                        if name == "body" {
                            if !self.body.get() {
                                // What came before the body (a head left open) isn't the message.
                                out.clear();
                                self.body.set(true);
                                out.push_str("<div");
                                self.attrs("body", &tag.attrs, &mut out);
                                out.push('>');
                            }
                            return TokenSinkResult::Continue;
                        }
                        if DROPPED.contains(&name.as_str()) {
                            *skip = Some((name.clone(), 1));
                            return raw(&name);
                        }
                        if UNWRAPPED.contains(&name.as_str()) || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':') {
                            return TokenSinkResult::Continue;
                        }
                        out.push('<');
                        out.push_str(&name);
                        self.attrs(&name, &tag.attrs, &mut out);
                        out.push('>');
                        if out.len() > HTML_MAX {
                            self.ended.set(true);
                        }
                        raw(&name)
                    }
                    TagKind::EndTag => {
                        if name == "body" {
                            if self.body.get() {
                                out.push_str("</div>");
                                self.ended.set(true);
                            }
                        } else if !(VOID.contains(&name.as_str())
                            || UNWRAPPED.contains(&name.as_str())
                            || DROPPED.contains(&name.as_str())
                            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ':'))
                        {
                            out.push_str("</");
                            out.push_str(&name);
                            out.push('>');
                        }
                        TokenSinkResult::Continue
                    }
                }
            }
            Token::CharacterTokens(t) if skip.is_none() => {
                let mut out = self.out.borrow_mut();
                esc_text(&t, &mut out);
                if out.len() > HTML_MAX {
                    self.ended.set(true);
                }
                TokenSinkResult::Continue
            }
            // Comments (Outlook's `<!--[if mso]>` among them), the doctype, stray NULs.
            _ => TokenSinkResult::Continue,
        }
    }

    fn end(&self) {
        // A body never closed is closed here.
        if self.body.get() && !self.ended.get() {
            self.out.borrow_mut().push_str("</div>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(html: &str) -> String {
        clean_html(html, &mut |_| None)
    }

    #[test]
    fn quoted_html_strips_scripts_and_handlers() {
        let html = r##"<!doctype html><html><head><title>T</title><meta charset="utf-8"><style>p{color:red}</style>
<script>alert(1)</script></head><body bgcolor="#fff" onload="x()"><!--[if mso]><table><tr><td>MSO only</td></tr></table><![endif]-->
<p onclick="steal()">Hello <b>there</b> &amp; <a href="javascript:alert(1)">click</a> <a href=" JaVa&#x09;script:x">two</a>
<a href="https://example.com/?a=1&amp;b=2">site</a></p>
<img src="x.png" onerror="alert(1)"><iframe src="https://evil.example">inside</iframe>
<form action="https://evil.example"><input name=q value="v"><button>Go</button>words</form>
<div style="width:expression(alert(1))">expr</div><div style="background:url(cid:a@x)">bg</div>
<script>document.write("<p>late</p>")</script><svg><script>x()</script></svg>
</body></html><p>after</p>"##;
        let out = clean(html);
        for gone in [
            "<script",
            "alert",
            "onload",
            "onclick",
            "onerror",
            "javascript",
            "<iframe",
            "inside",
            "<form",
            "<input",
            "<button",
            "MSO only",
            "<!--",
            "<style",
            "<title",
            "<meta",
            "expression",
            "after",
            "late",
            "steal",
        ] {
            assert!(!out.to_ascii_lowercase().contains(&gone.to_ascii_lowercase()), "{gone} left in {out}");
        }
        for kept in [
            "Hello",
            "<b>there</b>",
            "&amp;",
            ">click</a>",
            ">two</a>",
            "href=\"https://example.com/?a=1&amp;b=2\"",
            "words",
            "Go",
            "<img src=\"x.png\">",
            "url(cid:a@x)",
            "bgcolor=\"#fff\"",
        ] {
            assert!(out.contains(kept), "{kept} missing from {out}");
        }
        assert!(out.starts_with("<div") && out.trim_end().ends_with("</div>"), "{out}");
        // Not a document: all of it.
        assert_eq!(clean("<p>Hi <i>you</i></p>"), "<p>Hi <i>you</i></p>");
        // Text that looks like markup stays text.
        assert_eq!(clean("a &lt;script&gt; b"), "a &lt;script&gt; b");
        // Junk never panics.
        for junk in ["", "<", "<<<>>>", "<p", "</body>", "<body><body>x", "<script>", "<style>", "<a href=\"", "\u{0}<b>\u{0}</b>"] {
            let _ = clean(junk);
        }
    }

    #[test]
    fn picture_addresses_are_rewritten() {
        let mut seen = Vec::new();
        let out = clean_html(
            r#"<img src="cid:logo@x"><td background="cid:bg@x"><div style="background-image: url('cid:hero@x')">x</div><img src="https://web.example/p.png">"#,
            &mut |src| {
                seen.push(src.to_string());
                src.starts_with("cid:").then(|| format!("cid:new-{}", src.trim_start_matches("cid:")))
            },
        );
        assert!(
            out.contains("src=\"cid:new-logo@x\"") && out.contains("background=\"cid:new-bg@x\"") && out.contains("url('cid:new-hero@x')"),
            "{out}"
        );
        assert!(out.contains("src=\"https://web.example/p.png\""), "{out}");
        assert_eq!(seen.len(), 4);
    }
}
