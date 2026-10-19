//! An address book built from mail already in the cache, for To/Cc suggestions.
//!
//! Every sender and recipient seen counts, and everyone you sent mail to is remembered when it
//! goes ([`SentTo`]), so they're suggested even when the copy in Sent Items isn't downloaded.
//! People you write to (the recipients of your own
//! messages) rank well above people who only write to you, and recent contact counts more than
//! old: score = 4 · sent + received, halved for every year since the last message.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{EmailAddress, Summary};

/// One person.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contact {
    pub address: EmailAddress,
    /// Messages you sent them.
    pub sent: u32,
    /// Messages from them (or with them in copy).
    pub received: u32,
    /// The newest message involving them (Unix seconds).
    pub last: i64,
}

impl Contact {
    fn score(&self, now: i64) -> f64 {
        let years = (now.saturating_sub(self.last)).max(0) as f64 / (365.0 * 86_400.0);
        (4.0 * f64::from(self.sent) + f64::from(self.received)) * 0.5f64.powf(years)
    }
}

/// Most addresses seen; a mailbox full of newsletters stays bounded.
const MAX_CONTACTS: usize = 20_000;

/// Most people remembered as written to ([`remember_sent_to`]); the longest unused go first.
pub const MAX_SENT_TO: usize = 5_000;

/// Someone you sent mail to, remembered when the message went (To, Cc and Bcc), apart from the
/// cache: suggested whether or not the copy in Sent Items is ever downloaded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SentTo {
    pub address: EmailAddress,
    /// The last time (Unix seconds).
    pub last: i64,
    /// How many messages.
    pub times: u32,
}

/// Note the recipients of a message sent `now` in `list`, leaving out `mine` (the user's own
/// addresses) and what isn't an address; what changed comes back, for a book already built.
pub fn remember_sent_to(list: &mut Vec<SentTo>, recipients: &[EmailAddress], mine: &[String], now: i64) -> Vec<SentTo> {
    let mut noted = Vec::new();
    for a in recipients {
        let email = a.email.trim().to_ascii_lowercase();
        if !crate::addr::is_valid_email(&email) || mine.iter().any(|m| m.trim().eq_ignore_ascii_case(&email)) {
            continue;
        }
        if noted.iter().any(|n: &SentTo| n.address.email == email) {
            continue;
        }
        let name = a.name.trim().to_string();
        match list.iter_mut().find(|s| s.address.email == email) {
            Some(s) => {
                if !name.is_empty() {
                    s.address.name = name.clone();
                }
                s.last = s.last.max(now);
                s.times = s.times.saturating_add(1);
            }
            None => list.push(SentTo { address: EmailAddress::new(name.clone(), email.clone()), last: now, times: 1 }),
        }
        noted.push(SentTo { address: EmailAddress::new(name, email), last: now, times: 1 });
    }
    if list.len() > MAX_SENT_TO {
        list.sort_by_key(|s| std::cmp::Reverse(s.last));
        list.truncate(MAX_SENT_TO);
    }
    noted
}

#[derive(Clone, Debug, Default)]
pub struct ContactBook {
    by_email: HashMap<String, Contact>,
}

impl ContactBook {
    pub fn new() -> ContactBook {
        ContactBook::default()
    }

    pub fn len(&self) -> usize {
        self.by_email.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_email.is_empty()
    }

    /// What the book knows about an address.
    pub fn get(&self, email: &str) -> Option<&Contact> {
        self.by_email.get(&email.trim().to_ascii_lowercase())
    }

    fn note(&mut self, a: &EmailAddress, date: i64, sent: bool, times: u32) {
        let email = a.email.trim().to_ascii_lowercase();
        if !crate::addr::is_valid_email(&email) || email.contains("noreply") || email.contains("no-reply") || email.starts_with("mailer-daemon@") {
            return;
        }
        if self.by_email.len() >= MAX_CONTACTS && !self.by_email.contains_key(&email) {
            return;
        }
        let c = self.by_email.entry(email.clone()).or_insert_with(|| Contact { address: EmailAddress::new("", email), ..Default::default() });
        // The newest non-empty name wins (people fix their display names).
        if !a.name.trim().is_empty() && (c.address.name.is_empty() || date >= c.last) {
            c.address.name = a.name.trim().to_string();
        }
        if sent {
            c.sent = c.sent.saturating_add(times);
        } else {
            c.received = c.received.saturating_add(times);
        }
        c.last = c.last.max(date);
    }

    /// Someone saved in an address book: ranked as if written to often.
    pub fn add_saved(&mut self, a: &EmailAddress, now: i64) {
        self.note(a, now, true, 8);
    }

    /// Someone you sent mail to, as remembered when it went.
    pub fn add_sent_to(&mut self, s: &SentTo) {
        self.note(&s.address, s.last, true, s.times.max(1));
    }

    /// Count a message. `mine` are the user's own addresses: a message from one of them is one
    /// you sent, so its recipients are people you write to.
    pub fn add(&mut self, m: &Summary, mine: &[String]) {
        let from_me = mine.iter().any(|e| e.eq_ignore_ascii_case(m.from.email.trim()));
        if from_me {
            self.add_from_sent(m, mine);
        } else {
            self.note(&m.from, m.date, false, 1);
        }
    }

    /// Count a message you sent (one in Sent Items, whichever of your addresses or aliases it
    /// went from): its recipients are people you write to.
    pub fn add_from_sent(&mut self, m: &Summary, mine: &[String]) {
        for a in m.to.iter().chain(&m.cc) {
            if !mine.iter().any(|e| e.eq_ignore_ascii_case(a.email.trim())) {
                self.note(a, m.date, true, 1);
            }
        }
    }

    /// Up to `limit` contacts matching `query` (the start of the address, of the name, or of any
    /// word of the name; case-insensitive), best first.
    pub fn suggest(&self, query: &str, limit: usize, now: i64) -> Vec<Contact> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(&Contact, f64)> = self
            .by_email
            .values()
            .filter(|c| {
                let name = c.address.name.to_lowercase();
                c.address.email.starts_with(&q)
                    || name.starts_with(&q)
                    || name.split(|ch: char| ch.is_whitespace() || ch == '-' || ch == '.').any(|w| w.starts_with(&q))
                    || c.address.email.split(['.', '_', '-', '@']).any(|w| w.starts_with(&q))
            })
            .map(|c| (c, c.score(now)))
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.address.email.cmp(&b.0.address.email)));
        hits.into_iter().take(limit).map(|(c, _)| c.clone()).collect()
    }
}

impl ContactBook {
    /// Everyone known whose name or address contains `query` (everyone for an empty one), by
    /// name: the address book's list.
    pub fn list(&self, query: &str, limit: usize) -> Vec<Contact> {
        let q = query.trim().to_lowercase();
        let mut v: Vec<Contact> = self
            .by_email
            .values()
            .filter(|c| q.is_empty() || c.address.name.to_lowercase().contains(&q) || c.address.email.to_lowercase().contains(&q))
            .cloned()
            .collect();
        let key = |c: &Contact| if c.address.name.trim().is_empty() { c.address.email.to_lowercase() } else { c.address.name.to_lowercase() };
        v.sort_by_key(key);
        v.truncate(limit);
        v
    }
}

/// The address being typed in a recipient box: the text after the last `,` or `;`.
pub fn last_token(field: &str) -> &str {
    field.rsplit([',', ';']).next().unwrap_or("").trim_start()
}

/// `field` with its last, half-typed address replaced by `a` and a separator for the next one.
pub fn complete(field: &str, a: &EmailAddress) -> String {
    let keep = match field.rfind([',', ';']) {
        Some(i) => format!("{}, ", field.get(..i).unwrap_or("").trim_end()),
        None => String::new(),
    };
    format!("{keep}{a}, ")
}

/// Backspace right after a whole recipient (`Name <address>`, or one ended by `,` or `;`):
/// the field without it and where the cursor goes (characters), as Outlook takes a resolved name
/// away in one go. `None` when the cursor is inside a half-typed address (an ordinary backspace).
pub fn remove_recipient(field: &str, cursor: usize) -> Option<(String, usize)> {
    let chars: Vec<char> = field.chars().collect();
    let cursor = cursor.min(chars.len());
    // Separators outside quotes and angle brackets.
    let mut seps: Vec<usize> = Vec::new();
    let (mut quoted, mut angle) = (false, false);
    for (i, c) in chars.iter().enumerate().take(cursor) {
        match c {
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => seps.push(i),
            _ => {}
        }
    }
    if quoted || angle {
        return None;
    }
    let mut end = cursor;
    while end > 0 && chars.get(end - 1).is_some_and(|c| c.is_whitespace()) {
        end -= 1;
    }
    let last = *chars.get(end.checked_sub(1)?)?;
    let entry_end = match last {
        ',' | ';' => end - 1,
        '>' => end,
        _ => return None,
    };
    let start = seps.iter().rev().find(|s| **s < entry_end).map_or(0, |s| s + 1);
    let entry: String = chars.get(start..entry_end)?.iter().collect();
    if entry.trim().is_empty() {
        return None;
    }
    // A plain address ended by a separator counts too; a half-typed word before `>` doesn't.
    if last == '>' && !entry.contains('<') {
        return None;
    }
    let keep_to = if start == 0 { 0 } else { start };
    let before: String = chars.get(..keep_to).unwrap_or(&[]).iter().collect();
    let after: String = chars.get(cursor..).unwrap_or(&[]).iter().collect();
    let before = if after.trim().is_empty() && keep_to > 0 { format!("{} ", before.trim_end()) } else { before };
    let at = before.chars().count();
    Some((format!("{before}{}", after.trim_start()), at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessageKey;

    /// Issue #1: a wrong address picked from the suggestions goes with one Backspace.
    #[test]
    fn backspace_takes_a_whole_recipient() {
        let f = "Ana Lima <ana@x.example>, Zoe <zoe@x.example>, ";
        let (out, at) = remove_recipient(f, f.chars().count()).unwrap();
        assert_eq!(out, "Ana Lima <ana@x.example>, ");
        assert_eq!(at, out.chars().count());
        let (out, _) = remove_recipient(&out, out.chars().count()).unwrap();
        assert_eq!(out, "");
        let f = "\"Park, Zoe\" <zoe@x.example>";
        assert_eq!(remove_recipient(f, f.chars().count()).unwrap().0, "", "a comma in a quoted name isn't a separator");
        assert_eq!(remove_recipient("ana@x.example; ", 15).unwrap().0, "");
        // Half-typed, or inside an address: an ordinary backspace.
        assert_eq!(remove_recipient("Ana Lima <ana@x.example>, zo", 28), None);
        assert_eq!(remove_recipient("Ana <ana@x.exa", 14), None);
        assert_eq!(remove_recipient("", 0), None);
        assert_eq!(remove_recipient(", ", 2), None);
        // In the middle: the one before the cursor goes, the rest stays.
        let f = "Ana <a@x.example>, Zoe <z@x.example>";
        let (out, at) = remove_recipient(f, 18).unwrap();
        assert_eq!((out.as_str(), at), ("Zoe <z@x.example>", 0));
        assert_eq!(remove_recipient("abc", 99), None);
    }

    fn msg(from: (&str, &str), to: &[(&str, &str)], date: i64) -> Summary {
        Summary {
            key: MessageKey::default(),
            from: EmailAddress::new(from.0, from.1),
            to: to.iter().map(|(n, e)| EmailAddress::new(*n, *e)).collect(),
            date,
            ..Default::default()
        }
    }

    #[test]
    fn people_you_write_to_come_first() {
        let me = vec!["me@studio.example".to_string()];
        let mut b = ContactBook::new();
        let now = 2_000_000_000;
        for _ in 0..3 {
            b.add(&msg(("Zara Newsletter", "zara@news.example"), &[("Me", "me@studio.example")], now - 100), &me);
        }
        b.add(&msg(("Me", "me@studio.example"), &[("Zoe Park", "zoe@studio.example")], now - 100), &me);
        b.add(&msg(("Me", "me@studio.example"), &[("Zoe Park", "ZOE@studio.example")], now - 50), &me);
        b.add(&msg(("Robot", "noreply@x.example"), &[], now), &me);
        assert_eq!(b.len(), 2, "no-reply senders and my own address are left out");
        let s = b.suggest("z", 5, now);
        let emails: Vec<&str> = s.iter().map(|c| c.address.email.as_str()).collect();
        assert_eq!(emails, vec!["zoe@studio.example", "zara@news.example"]);
        assert_eq!(s[0].sent, 2);
        assert_eq!(s[0].address.name, "Zoe Park");
        // Any word of the name, or the domain, matches.
        assert_eq!(b.suggest("park", 5, now).len(), 1);
        assert_eq!(b.suggest("news", 5, now).len(), 1);
        assert!(b.suggest("  ", 5, now).is_empty());
        assert!(b.suggest("q", 5, now).is_empty());
    }

    #[test]
    fn people_sent_to_are_remembered_once_each_and_bounded() {
        let mine = vec!["me@x.example".to_string()];
        let mut list = Vec::new();
        let r = [
            EmailAddress::new("Ann", "Ann@Y.example"),
            EmailAddress::new("", "ann@y.example"),
            EmailAddress::new("Me", "ME@x.example"),
            EmailAddress::new("", "not an address"),
        ];
        let noted = remember_sent_to(&mut list, &r, &mine, 10);
        assert_eq!(noted.len(), 1);
        assert_eq!(list, vec![SentTo { address: EmailAddress::new("Ann", "ann@y.example"), last: 10, times: 1 }]);
        remember_sent_to(&mut list, &[EmailAddress::new("", "ann@y.example")], &mine, 20);
        assert_eq!((list[0].address.name.as_str(), list[0].last, list[0].times), ("Ann", 20, 2), "the name stays when none is given");
        let mut book = ContactBook::new();
        book.add_sent_to(&list[0]);
        assert_eq!(book.get("ann@y.example").map(|c| c.sent), Some(2));
        for i in 0..MAX_SENT_TO + 10 {
            remember_sent_to(&mut list, &[EmailAddress::new("", format!("p{i}@z.example"))], &mine, 100 + i as i64);
        }
        assert_eq!(list.len(), MAX_SENT_TO);
        assert!(!list.iter().any(|s| s.address.email == "ann@y.example"), "the longest unused go first");
    }

    #[test]
    fn old_contacts_fade() {
        let me = vec!["me@x.example".to_string()];
        let mut b = ContactBook::new();
        let year = 365 * 86_400;
        let now = 10 * year;
        b.add(&msg(("Me", "me@x.example"), &[("Ann Old", "ann.old@x.example")], now - 5 * year), &me);
        b.add(&msg(("Ann New", "ann.new@x.example"), &[], now), &me);
        let s = b.suggest("ann", 5, now);
        assert_eq!(s[0].address.email, "ann.new@x.example");
    }

    #[test]
    fn completing_the_last_address() {
        assert_eq!(last_token("Ann <a@x>, zo"), "zo");
        assert_eq!(last_token("zo"), "zo");
        assert_eq!(last_token("a@x; "), "");
        let z = EmailAddress::new("Zoe Park", "zoe@x.example");
        assert_eq!(complete("Ann <a@x>, zo", &z), "Ann <a@x>, Zoe Park <zoe@x.example>, ");
        assert_eq!(complete("zo", &z), "Zoe Park <zoe@x.example>, ");
    }
}
