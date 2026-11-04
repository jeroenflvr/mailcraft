# Task: four things a Microsoft 365 user hits every day

Repository: `~/projects/mailcraft`. Read `AGENTS.md` first and follow it (no panics, no `unsafe`,
everything is a command, `tl!`/`tf!` for every string, `cargo xtask ci` before each commit,
commit after each arc). Work autonomously; there are no owner decisions in this task. Never
test against a real account: the fake servers (`crates/net/src/fake_graph.rs`, `fake.rs`) and
the sample mailbox (`--sample`) cover everything below.

Four problems, each investigated and traced in the code already (**don't re-investigate; fix**).
Do them in this order — the first two make the client unusable with Exchange Online, the other
two make it look worse than Outlook — and commit each one on its own:

1. [Signing in again all the time (Graph / Microsoft 365)](#1-signing-in-again-all-the-time)
2. [A 300 KB message takes two minutes to download](#2-a-300-kb-message-takes-two-minutes-to-download)
3. [Forward loses the message's formatting](#3-forward-loses-the-messages-formatting)
4. [The signature can't hold a logo and can't be formatted properly](#4-the-signature-cant-hold-a-logo-and-cant-be-formatted-properly)

Line numbers below are from the current tree (`34a92eb`); they drift as you edit — search for
the symbol.

---

## 1. Signing in again all the time

### What happens (verified)

- `crates/net/src/worker.rs::Worker::connect` (~1190) gets an access token from
  `Worker::secret()` (~748) **once**, and builds `graph::Client::connect(url, &secret)`
  (`crates/net/src/graph.rs` ~183–192), which stores `auth: format!("Bearer {token}")` in the
  `Client` struct (~67–75). That `Conn::Graph(c)` then lives for the whole session: the main
  loop (`run`, ~833–866) keeps it across every sync, poll and job; only an error drops it.
- Microsoft access tokens last 60–90 minutes. After that, the next Graph request answers
  **401**, `status_error` (graph.rs ~98) turns it into `NetError::Auth("Microsoft refused the
  sign-in; sign in again")`, and `Worker::failed_in` (worker.rs ~1175–1179) does
  `self.auth_failed = true; self.status(NeedsPassword)`. The main loop then blocks on
  `rx.recv()` (~842–847) until the user signs in in the browser again. **The refresh token in
  the keychain is never used at that point**, although `secret()` knows how to refresh.
- So: the user has to sign in again roughly every hour, or after the laptop sleeps
  (`Tokens.expires_at` is an `Instant`, oauth.rs ~177: monotonic time doesn't advance during
  suspend, so a token that "has 40 minutes left" after a 2-hour sleep is long dead).
- The same shape exists for `Conn::Ews` (token in the EWS client) and `Conn::Jmap` with OAuth;
  fix them with the same mechanism (the worker-level part below covers them; add
  `set_token` to those clients where they keep the token).
- Second, independent cause: `oauth::token_request` (oauth.rs ~150–158) turns **every**
  non-2xx answer from the token endpoint into `NetError::Auth` — a 5xx, a 429, a captive
  portal page without JSON (`Protocol`, fine) — so a hiccup at `login.microsoftonline.com`
  while refreshing is reported as "sign in again" too.
- Third, to check with the user's log, not to fix in code: on a desktop without a Secret
  Service daemon (gnome-keyring, KWallet, KeePassXC; `NIXOS.md` § Keychain describes the
  symptom) the keychain write fails; the initial sign-in then says "the sign-in couldn't be
  kept in the keychain" (worker.rs ~232) and every start needs a new sign-in. The user here is
  on Debian trixie / LMDE 7 (Cinnamon, gnome-keyring), so this is **unlikely** — but have the
  log line `needs signing in again: … keychain …` checked once (`secret-tool search service
  MailCraft` lists the saved entries), and make that message point at troubleshooting (see
  *Docs* below).

### The rule

**A token's lifetime is not the connection's lifetime.** A Graph/EWS/JMAP connection asks the
worker for a current token before every piece of work; a token near its end is refreshed
*before* it is used; a 401 means "refresh and try once more", and only a refusal **of the
refresh itself** (`invalid_grant`, `interaction_required`, `consent_required`,
`invalid_client`, `unauthorized_client`) means "sign in again". Anything else that goes wrong
while refreshing is a network error with backoff, and the stored refresh token is kept.

### Design

**`crates/net/src/oauth.rs`**
- `Tokens`: add `expires_at_wall: u64` (Unix seconds, `SystemTime`), keep `expires_at:
  Instant` too; `fn expired(&self, margin: Duration) -> bool` is true when **either** clock
  says so. `net` is native-only, so `SystemTime` is fine here (never in portable crates).
- `token_request`: split the error. When the body has `"error"`: if it is one of the
  definitive codes above → `NetError::Auth(…)` (as now). Any other error code, or a non-2xx
  without `error` (5xx, 429, 408) → `NetError::Closed(format!("{}: the sign-in service
  answered {status}: {what}", p.name))` (retryable). Keep the `Protocol` error for non-JSON.
  Add tests: `a_500_from_the_token_endpoint_is_not_a_sign_in_failure`,
  `invalid_grant_still_is` (the fake token server pattern already in the file's tests
  ~330–380).

**`crates/net/src/graph.rs`** (and the EWS/JMAP clients where they hold a bearer token)
- `Client::set_token(&mut self, token: &str)` rebuilds `auth`. Nothing else in the client
  changes; `connect` stays as the one place that probes `/me/mailFolders/inbox`.
- Keep `status_error`'s 401 → `NetError::Auth`; the retry lives in the worker (it owns the
  refresh token and the mutex).

**`crates/net/src/worker.rs`**
- `Worker::secret()` already serialises refreshes behind `self.auth` and persists a rotated
  refresh token; keep it. Add `fn fresh_token(&mut self) -> Result<String>` = `secret()` but
  with the margin raised to **5 minutes** (`expired(Duration::from_secs(300))`), and
  `fn forget_token(&self)` that clears `auth.access` under the lock.
- `fn refresh_conn_token(&mut self) -> Result<()>`: for OAuth accounts with a live `conn`,
  calls `fresh_token()` and, if the token string changed, `set_token` on `Conn::Graph` /
  `Conn::Ews` / `Conn::Jmap`. Call it at the top of `handle(job)`, at the start of
  `sync_all`, before `sync_one` in `wait_for_mail`'s poll path, and in `run_quick` before
  `send_quick`/`handle`. A refresh failure here goes through `failed_in` like any error (so
  a `Closed` backs off, an `Auth` asks to sign in).
- **401 once → refresh → retry once.** Add `retried_token_at: Option<Instant>` to `Worker`.
  In `failed_in`'s `NetError::Auth(m)` arm, before the generic branch: if the account is
  OAuth (`self.acc.oauth.is_some()`), the token came from a cache (`auth.access.is_some()`),
  and `retried_token_at` is `None` or older than 10 minutes → `forget_token()`,
  `self.conn = None`, `retried_token_at = Some(now)`, `next_attempt = now`, **don't** set
  `auth_failed`, and log `"{id}: the token was refused; getting a new one"`. The main loop
  reconnects on the next turn (`connect()` → `secret()` → refresh). The job that failed must
  not be lost:
  - `handle(job)`: wrap the match so that on this "retrying with a new token" outcome the job
    is `push_front`ed back onto `self.queue` (add a `fn token_retry_pending(&self) -> bool`
    the arms consult, or make `failed_in` return an enum `Failed::{Retrying, Done}`).
  - `fetch(key)` (~1801): on `NetError::Auth` first time, don't emit `BodyFailed`; requeue.
  - `sync_all`/`sync_one`: nothing to requeue — `last_full` is untouched, so the full sync
    runs again after reconnecting; `wait_for_mail`'s poll just polls again.
  - If the **second** attempt also gets `Auth` (within 10 minutes) → the existing path:
    `auth_failed = true`, `NeedsPassword`.
- When `secret()`'s refresh fails with `NetError::Closed` (transient), `connect()` fails with
  it; the existing `e if e.is_fatal()` arm backs off (5 s → 300 s). Check `is_fatal()` covers
  `Closed` (lib.rs ~93) so the account shows "Error" and retries instead of "Sign in again".
- Keychain key: `apps/mailcraft/src/main.rs::entry` (~38–40) keys the secret by
  `"{id}|{username}|{imap.host}"`. For a Graph account `imap.host` may change when the user
  edits server settings, and the refresh token then "vanishes". Switch to
  `"{id}|{username}"` **with a one-time fallback read of the old key** (`get` tries the new
  key, then the old one and, if found there, re-saves under the new key and deletes the old).

**UI text** (`crates/ui-egui`): nothing new unless you add a notice; "Sign in again" already
exists for OAuth accounts (`docs/gaps.md` ~200).

### Tests (`crates/net/src/worker.rs` tests, `fake_graph.rs`)
- `an_expired_graph_token_is_refreshed_without_asking`: the fake Graph server accepts token
  `t1`, then (after a flag flips) answers 401 to `t1` and 200 to `t2`; a fake token endpoint
  (reuse the one in `oauth.rs` tests, or add a `token_url` override on `Provider` for tests)
  hands out `t2` for refresh token `r1`. Start a worker with `oauth: Some("microsoft")`,
  secret `r1`, run a sync, flip the flag, submit `FetchBody`: assert the body arrives, **no**
  `NeedsPassword` event, exactly one refresh request, and the Graph request was retried once.
- `a_token_near_its_end_is_refreshed_before_use`: set `expires_at` 2 minutes ahead; the next
  job refreshes first and the fake Graph server sees only `t2`.
- `a_second_401_asks_to_sign_in`: fake Graph answers 401 to everything → one refresh, then
  `NeedsPassword`.
- `a_refresh_that_fails_with_a_500_backs_off`: assert `SyncStatus::Error`, not
  `NeedsPassword`, and that `auth.secret` (the refresh token) is unchanged.
- `the_keychain_key_falls_back_to_the_old_one` in `apps/mailcraft` (unit test with the
  `keyring` mock store, or factor the key function so it's testable without a keyring).

### Docs
- `docs/oauth.md`: a paragraph "Staying signed in": tokens are refreshed five minutes before
  they end and after a 401; which refusals mean signing in again.
- `docs/troubleshooting.md`: "Asked to sign in again every hour / after sleep" → fixed in this
  version; "at every start" → the keychain (no Secret Service running; `NIXOS.md` § Keychain
  for NixOS), and the log line to look for.
- `docs/gaps.md`: a line under the version's heading.

---

## 2. A 300 KB message takes two minutes to download

### Where the time goes (verified; the body reader is *not* the problem)

`http::read_response` (`crates/net/src/http.rs` ~331–409) reads 16 KiB at a time with
`TCP_NODELAY` on; the TLS stream is rustls' `StreamOwned`; Graph fetches the message as raw
MIME (`/me/messages/{id}/$value`, graph.rs ~400–407). None of that explains minutes. These do:

1. **The download waits in line behind the sync.** `Job::FetchBody` goes to the account's
   **main** connection (`quick_job`, worker.rs ~120–126, only sends `Send`/vacation to the
   second connection). The main connection handles it only when it gets back to its queue:
   - during a full sync (`sync_all`, ~1379–1425) urgent jobs are handled **between folders**
     (~1395–1399) but never *inside* a folder's sync;
   - `graph::sync_folder` (~599–693) for one folder does: `newest()` (one request per 1000
     ids of the window; ~377–390), `headers()`/listing batches, **then `prefetch()`
     (~695–725): up to `opts.prefetch` (default 40, `crates/jmap/src/lib.rs` ~677)
     `$value` downloads, one after another**, each a Graph MIME export that takes 0.5–3 s
     server-side — and nothing checks the queue in that loop;
   - Graph has no push: `wait_for_mail` (~1020–1050) polls the Inbox every
     `check_minutes` (default **2 minutes**, `crates/model/src/job.rs` ~54). A message
     that the phone shows at once appears here up to two minutes later, and when the poll
     fires the Inbox sync (with its prefetch) starts — exactly when the user clicks the new
     message.
   A 2-minute wait is therefore ordinary: the click lands while the connection is in a folder
   sync, and the body is fetched after it.
2. **Every Graph request opens a new connection.** `Client::send_patiently` (graph.rs
   ~164–180) calls `http::send` → `once()` (http.rs ~154–197): DNS lookup, TCP connect, full
   TLS handshake, `Connection: close` — for *each* of the dozens of requests in a sync and
   each prefetch. `http::Conn` (keep-alive, ~225–262) exists and Graph doesn't use it.
3. **Retries on 502/503/504 for GETs** (`throttled`, ~108–110): Graph's MIME export of a
   larger message answers 504 fairly often; `retry_wait` then sleeps 1, 2, 4, 8, 16 s
   (`MAX_WAIT_SECS` 30) up to `RETRIES` = 5 times, each attempt itself up to `IO_TIMEOUT`
   (60 s, `stream.rs` ~37). Worst case: minutes for one message, silently (`log::info!`
   "Graph is busy").

### The rule

**What the user is waiting for never waits behind what they aren't.** Opening a message
fetches it now, on a connection that isn't busy, and the user can see where the time went.

### Design

**A. Fetching a body jumps every queue (worker.rs)**
- Route `Job::FetchBody` to the **second connection** for every account that has one
  (`quick_job`: `Job::FetchBody { .. } => acc.pop.is_none()`), and handle it in `run_quick`
  (`j @ Job::FetchBody { .. } => self.handle(j)`). The second connection for Graph/EWS/JMAP
  is just another HTTP session — free. IMAP servers allow a few connections (Gmail 15); the
  second one is already used for sending.
- The second worker's `jstate` (the uid ↔ server-id map, loaded once at start, ~698) is
  stale for a message that arrived later. In `fetch()` on the quick worker (`self.main.is_some()`),
  when `jstate` has no id for `key.uid`, reload it: `self.jstate = jmap::State::load(&self.store,
  &self.acc.id)` (the main worker saves after every folder, `save_jstate` ~1328) and try
  again; if still unknown, hand the job to the main connection (`self.main.send(job)`), as
  `run_quick` does for jobs it can't do.
- Keep the main connection's existing urgent handling as the fallback when `connections.second`
  is off. And make a folder sync interruptible: give `graph::sync_folder`, `jmap::sync_folder`,
  `ews::sync_folder` and `sync::sync_folder` (IMAP) a `pause: &mut dyn FnMut() -> bool`
  parameter that `prefetch()` consults **before each download**; when it returns true the
  prefetch stops (returns `Ok`; the rest comes with the next sync). In `Worker::sync_one` the
  closure does `rx.try_recv()` into a local `Vec<Job>`, returns whether any is `FetchBody`
  or `Apply`, and after the sync those jobs go to the **front** of `self.queue`. (`SyncOptions`
  is `Copy`; don't put the closure in it.)
- Lower the second connection's idle timeout for nothing; `QUICK_IDLE` (5 min) is fine.

**B. One connection per Graph session (graph.rs + http.rs)**
- `Client` gets `conn: Mutex<Option<http::Conn>>`. `send_patiently` takes the lock, opens a
  `Conn` when there is none or `!usable()`/doesn't `reaches(url)`, sends on it, and on an
  I/O/`Closed` error **drops it and tries once more on a fresh connection** (a server closing
  an idle keep-alive is normal). `Conn::send` already marks `closing` from the server's
  headers. No redirects are followed on `Conn` — Graph doesn't redirect; keep `http::send`
  for the `$batch` body? No: everything goes through the same `Conn`.
- Keep `once()`'s timing log line, and add the same three numbers to `Conn::send` (connected
  = 0 ms when reused) so the debug log shows reuse.
- `Client` must stay `Send` (it's held by the worker thread; `Mutex<Option<Conn>>` is).
- `fake_graph.rs`: the fake server answers `Connection: close` on every response (~469) and
  handles one request per accepted connection (~481). Teach it keep-alive: loop reading
  requests on the accepted stream until the client closes or sends `Connection: close`, and
  drop the `Connection: close` header (keep a per-test switch to force close, so
  `a_closed_keep_alive_is_reopened_once` can exercise the reopen path). Count accepts and add
  `graph_requests_share_one_connection`: a sync of the fake mailbox accepts 1–2 connections,
  not one per request.

**C. Honest retries (graph.rs)**
- Cap the total time `send_patiently` may spend on one request's retries at 45 s (sum of waits
  and attempts); then return the last response so the caller reports "HTTP 504" instead of
  silently sleeping. Log retries at `warn` with the path and the attempt number.
- For `download()` specifically, after one 504 try `Prefer: outlook.body-content-type="html"`?
  No — `$value` has no such knob; just keep the cap. (Don't switch to JSON `contentBytes`:
  base64 + a request per attachment is slower.)

**D. New mail sooner (worker.rs, model)**
- `Connections::check_minutes` is clamped to ≥ 1 (`job.rs` ~46). For API accounts (Graph,
  EWS) poll the Inbox every **30 s** when the window has focus (a cheap request:
  `newest()` with the ids of the window is one page for the default 1000) — add
  `check_seconds` only if the UI already exposes it; otherwise hard-code `API_POLL = 30 s` for
  `acc.is_api()` and leave IMAP (IDLE) and JMAP (push) alone. Make the Inbox poll for Graph
  a **delta** query when cheap to add (`/me/mailFolders/{id}/messages/delta`, keep the
  `@odata.deltaLink` in `jstate`); if that's more than a day's work, skip it — the 30 s poll
  alone fixes the perceived lag.

**E. Measure it (worker.rs, graph.rs)**
- Note when a `FetchBody` arrives (`drain`/`push_job`: keep `HashMap<MessageKey, Instant>`
  on the worker) and log at `info` on completion:
  `"{id}: {folder}/{uid}: downloaded {kb} KB in {server_ms} ms after {queue_ms} ms in line"`.
  This is the line to ask the user for.
- Add a bench-style test against `fake_graph.rs`: 40 messages to prefetch with the fake
  server sleeping 50 ms per `$value`; submit `FetchBody` for one more message mid-sync and
  assert `Event::Body` arrives within 500 ms (with A in place) rather than after the
  prefetch.

### Tests
- `fetch_body_does_not_wait_for_a_folder_sync` (the bench above, in `worker.rs` tests with
  the fake Graph server).
- `a_prefetch_pauses_for_an_opened_message` (main connection only, `second: false`).
- `graph_requests_share_one_connection`; `a_closed_keep_alive_is_reopened_once`.
- `retries_stop_after_45_seconds` with a fake server answering 504 (use a 1 s `Retry-After`
  and shrink the cap through a `#[cfg(test)]` setter, so the test takes seconds, not minutes).
- `the_quick_connection_reloads_ids_it_does_not_know`.

### Docs
- `docs/sync.md`: "Opening a message" (second connection, pausing prefetch), "Graph polling"
  (30 s), "One connection per session".
- `docs/troubleshooting.md`: the log line from E and how to read it.
- `docs/gaps.md`: a line.

---

## 3. Forward loses the message's formatting

### What happens (verified — note the earlier assumption "it quotes plain text" is wrong)

- `mail.forward` → `Session::reply_compose(ReplyMode::Forward)` (`crates/engine/src/session.rs`
  ~2525–2563) → `mime::reply_doc` (`crates/mime/src/lib.rs` ~1115) → `quoted_original`
  (~1077–1104), which **does** use the formatted body: `rich::from_rich(&p.rich)` (~1099).
  `from_rich` (`crates/mime/src/rich.rs` ~464–484) keeps bold, italic, strike, mono,
  headings (as sizes), colour, highlight and links. So *some* formatting survives. What is
  lost — and what the user sees as "all formatting" on real Outlook/newsletter mail:
  1. **Pictures** become the text `[picture]` (rich.rs ~478); the original's inline
     pictures aren't carried (`reply_compose` copies only `!a.inline` attachments, session.rs
     ~2534–2540) and `Compose.inline` stays empty.
  2. **Tables and layout**: when the reading pane (and now Print) draws the message from
     `Parsed::layout` (`reading.rs` ~678–688, `cmd.rs::message_doc` ~1324), forward uses
     `Parsed::rich`, html2text's linearised rendering — columns stacked, cell colours gone.
     Most Outlook-made and all newsletter HTML is table-based.
  3. **Underline, alignment, lists, indents, font sizes and faces** (`Span` has none of these;
     `from_rich` gives every line `Para::default()`).
  4. **What's sent** is regenerated from the editor model by `Compose::bodies` →
     `rich::to_html` (session.rs ~531–543, rich.rs ~387): the recipient gets the flattened
     version even where the sender's screen showed more.
- `mime::build` (~984–1053) already writes `multipart/alternative` + `multipart/related` with
  `cid:` pictures; `mime::html_body(raw)` (~814) returns the original HTML;
  `layout::inline_css` (used by `parse`, ~721) folds `<style>` rules into attributes;
  `picture_attachment(p, src)` (~549) maps a `cid:`/name/`Content-Location` to an attachment
  index; `attachment_data(raw, index)` (~823) gives its bytes.

### The rule

**What Outlook does: the forwarded (and replied-to) original goes out as the sender received
it** — its HTML, its inline pictures — below the user's text; the editor shows the best
approximation it can edit, with the pictures in place. If the user edits *inside* the
original, what they see is what is sent.

### Design

**`crates/engine/src/session.rs` — `Compose`**
```rust
/// The original under a reply or forward, as it was received, to send as Outlook does.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub quoted: Option<Quoted>,

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quoted {
    /// The original's HTML body, cleaned (see below), with `cid:` pictures pointing at `pictures`.
    pub html: String,
    /// The header block (From/Sent/To/Cc/Subject) as HTML, as `quoted_original` writes it in text.
    pub header_html: String,
    /// The quoted section's text exactly as it was put into `body` (header lines + original),
    /// to tell whether the user changed it.
    pub text: String,
}
```
The pictures go into `Compose.inline` (so the editor draws them and `bodies()` ships them).

**`crates/mime/src/lib.rs`**
- `pub fn quoted_html(raw: &[u8], p: &Parsed) -> Option<(String, Vec<InlinePicture>)>`:
  `html_body(raw)` → `layout::inline_css` → take the inner HTML of `<body>` (whole thing when
  there's no `<body>`) → remove `<script>`, `<style>` (already inlined), `<link>`, `<meta>`,
  `<title>`, `<base>`, `<iframe>`, `<object>`, `<embed>`, `<form>`, `on*=` attributes and
  `javascript:` URLs, HTML comments (incl. `<!--[if mso]>`) — use the html5ever tokenizer the
  crate already uses in `layout.rs`, not regexes; cap at `HTML_MAX`. For each `<img src>`
  (and CSS `background`/`background-image` `url(...)` that `layout::background_pictures`
  finds): `picture_attachment(p, src)` → carry the attachment as
  `InlinePicture { cid: fresh, name, mime, data }` and rewrite `src` to `cid:<fresh>`; remote
  `http(s):` pictures stay as they are (they're the sender's; forwarding them is what Outlook
  does — **but** the reading pane's "load remote content" rule still applies when *we* show
  it); `data:` URIs stay. Cap pictures: 50 and 15 MB total; past that, leave the rest as is.
  Fresh cids come from `Session::new_cid()` — pass a closure or return the needed count;
  simplest: take `cid_for: &mut dyn FnMut() -> String`.
- `quoted_original` / `reply_doc`: in `from_rich`, a `Span` whose `image` maps to a carried
  picture becomes the `PICTURE` character with `Style { picture: Some(cid) }` rather than
  `[picture]` (a map `src → cid` parameter; keep `[picture]` for what wasn't carried). Return
  the header block's HTML as well (`<div style="border-top:1px solid #e1e1e1;…"><b>From:</b>
  …</div>`, matching `quoted_original`'s text), and the quoted `text` as inserted.
  `reply_doc`'s return type grows: `(Draft, rich::Styles, Option<Quoted>, Vec<InlinePicture>)`
  — or a small struct `ReplyDoc`; update `reply()` (~1109) and its callers (CLI, tests).
- `Compose::bodies()` (session.rs): with `quoted: Some(q)` and `!plain`:
  - if `body` ends with `q.text` (allow trailing whitespace; compare on `char` boundaries) →
    `html = to_html(body[..start_of_quote]) + q.header_html + q.html`, pictures = those used
    by the editable part **plus** every picture referenced by `q.html`;
  - else (the user edited the quote) → today's behaviour, `to_html(body)` with pictures as
    used (now including the originals' `cid:`s in place).
  - the `text/plain` alternative is `to_text(body)` either way.
- `reply_compose`: build `Quoted` for **Reply, Reply All and Forward** (Outlook quotes the HTML
  original in all three); attachments: for Forward, carry those **not** referenced as pictures
  (so a logo isn't both inline and attached); `forward_as_attachment` (~2565) keeps its
  behaviour (`c.set_text` must also clear `quoted` and `inline` — do it inside `set_text`).
- `compose_written()` (~1075) must still say "nothing written" for an untouched forward: it
  compares the body to the signature only — check a forward with the quote and signature
  counts as "not written" as today (the quote is part of the initial body), add an assert.
- `Compose::plain = true` drops `quoted` at send time (text only), as it does the HTML.

**`crates/ui-egui`**
- Nothing structural: the editor already draws `PICTURE` characters from `c.inline`
  (`rich_edit.rs` ~850–865). Check the editor's picture cache key (`compose-{cid}`) is unique
  per compose for the fresh cids. Pictures from the original must respect the remote-content
  rule only for `http(s)` sources — those aren't in `inline`, so nothing loads; fine.

### Tests (`crates/mime/src/lib.rs`, `crates/engine/src/tests.rs`)
- `a_forward_carries_the_original_html_and_its_pictures`: parse a `multipart/related` message
  with a table, a `cid:` logo and a PDF attachment (the fixtures near ~1380–1430 already have
  `cid:pic@x`); forward; assert `compose.quoted.is_some()`, `inline.len() == 1`,
  `attachments == [the PDF]`, the body holds a `PICTURE` char styled with the logo's cid, and
  `bodies().1` (the HTML) contains `<table` and `cid:` of the carried picture, and **not**
  `<script`.
- `editing_inside_the_quote_sends_what_is_seen`: change a word in the quoted part; assert the
  sent HTML is `to_html` output (no `<table`), still with the picture.
- `a_reply_quotes_the_html_too`; `plain_text_drops_the_quote`;
  `forward_as_attachment_has_no_quote`.
- `quoted_html_strips_scripts_and_handlers`: `<img onerror=…>`, `<a href="javascript:…">`,
  `<!--[if mso]>…<![endif]-->`, `<style>` → all gone, text kept.
- Extend `every_command_survives_junk_params` fixtures if `Quoted` affects command params (it
  doesn't; but run it).
- A headless UI frame of a forward with a picture (`crates/ui-egui/src/tests.rs`, the compose
  layouts) to make sure the picture draws.

### Docs
- `docs/architecture.md` § The session: one sentence on `Compose.quoted`.
- `docs/gaps.md`: a line ("Reply and Forward send the original as received…").

---

## 4. The signature can't hold a logo and can't be formatted properly

### What happens (verified)

- The signature editor (`crates/ui-egui/src/dialogs.rs::SignatureForm` ~429–450, drawn at
  ~1687–1715) is `rich_edit::body` **without the ribbon**: only Ctrl+B/I/U work. There is no
  size, colour, alignment, list, link or picture control. ("Can't format properly".)
- Pictures can't get in: `Action::InsertPicture`/`PastePicture` (`crates/ui-egui/src/lib.rs`
  ~1612–1626) run `compose.insertPicture`, which targets `Session::compose` (the message), not
  the dialog's own `Compose`; pasted HTML goes through `mime::pasted_html` (`crates/mime/src/lib.rs`
  ~369–384), which **removes every picture** (`line.retain(|sp| sp.image.is_none())`), and
  `data:` URIs are rejected upstream anyway (`picture_attachment` ~553).
- Storage is lossy: `account.setSignature` (`cmd.rs` ~1703–1717) keeps `signature` (text) and
  `signature_html` = `rich::to_html(text, styles)`; `Session::signature_rich` (session.rs
  ~2488–2497) reads it back through `pasted_html` → `html_to_rich` (html2text) →
  `from_rich`, which has no underline, alignment, list, indent or rule, and gives every
  paragraph `Para::default()`. So a centred, bulleted or underlined signature is flat after
  Save. There is no place at all for picture bytes (`AccountConfig`, `crates/model/src/lib.rs`
  ~118–133).
- `insert_signature` (cmd.rs ~1221–1255), `new_compose`, `reply_compose` and
  `style_signature` (session.rs ~2500–2560, ~3548) put the signature's text and styles into
  the message but never add pictures to `Compose.inline`.

### The rule

**The signature is a small message:** the same editor, the same ribbon, the same pictures;
saved exactly as edited (not through HTML), and put into every new message, reply and forward
with its pictures, which go out as `cid:` parts like any other.

### Design

**`crates/model/src/lib.rs` — `AccountConfig`**
```rust
/// The signature's look: `mime::rich::Styles` as JSON (the model doesn't know the type);
/// empty for a plain signature. Replaces `signature_html`, which is read once to migrate.
#[serde(default, skip_serializing_if = "String::is_empty")]
pub signature_styles: String,
/// The pictures in the signature (its `cid:`s), small: a logo or two.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub signature_pictures: Vec<SignaturePicture>,

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignaturePicture {
    pub cid: String,
    pub name: String,
    pub mime: String,
    /// The bytes, base64 (JSON keeps `Vec<u8>` as a list of numbers, four times the size).
    pub data: String,
}
```
Caps (in the engine): 8 pictures, 512 KB each, 2 MB together — a signature goes on every
message. Keep `signature` (the text) as the plain twin; keep reading `signature_html` when
`signature_styles` is empty (existing users), and clear it when saving.

**`crates/engine`**
- `Session::signature_rich(account) -> (String, Styles)`: `signature_styles` JSON when present
  (`serde_json::from_str`, then `styles.fit(&text)` — hostile input), else the
  `signature_html` path as today, else plain. Add
  `fn signature_pictures(&self, account) -> Vec<mime::InlinePicture>` (base64-decode; a
  picture that doesn't decode is skipped, never fatal).
- `style_signature(c, sig)` (and so `new_compose`, `reply_compose`, `insert_signature`): after
  splicing the styled block, push each signature picture into `c.inline` if its cid isn't
  there yet. Cids are per account and stable (`sig-{n}@{account-id}.mailcraft`), so inserting
  the signature twice doesn't duplicate the bytes; `bodies()` already ships only pictures the
  text uses.
- `account.setSignature` params: `signature` (text), `styles` (the `Styles` JSON object),
  `pictures` (`[{cid, name, mime, data: base64}]`), keep `html` for the CLI/agents (converted
  as today). Validate: `styles.fit(&text)`; pictures only `image/*`, the caps above, cids
  referenced by the styles only (drop the rest); junk → `CmdError::Params`. Update
  `every_command_survives_junk_params`' param doc (`.params(...)` on the spec, cmd.rs ~788).
- `pasted_html` keeps `data:image/…;base64,` pictures: a new
  `pub fn pasted_html_with_pictures(html, cid_for: &mut dyn FnMut() -> String) -> (String,
  Styles, Vec<InlinePicture>)` (cap 2 MB each; `pasted_html` stays as the no-pictures wrapper).
  Outlook's own clipboard HTML references signature images as `file:///…` paths we can't
  read — those still drop; the dialog's hint text says to use *Pictures* for them.
- Factor the picture checks out of `insert_picture` (cmd.rs ~1197–1216) into
  `pub fn picture_from_file(name: &str, data: Vec<u8>) -> Result<InlinePicture, CmdError>` so
  the UI can validate a picture for the signature editor the same way.

**`crates/ui-egui`**
- `rich_edit::ribbon` (~939–1030): split into `format_bar(ui, t, ed, c, acts)` (size, B/I/U/S,
  highlight, colour, lists, indent, alignment, clear; plus Link and Pictures) and the message
  half (Address Book, Attach, Signature menu). `ribbon` = both. The Signature dialog draws
  `format_bar` above its editor; widen it to ~600 px.
- Pictures into the dialog: in `Action::InsertPicture` / `Action::PastePicture` /
  `Dialog::Signature` paste handling, when `self.ui.dialog` is `Dialog::Signature(f)`, read
  the file(s) through `Services` as today, validate with `picture_from_file`, give it a cid
  from `Session::new_cid()`, push into `f.compose.inline` and place it with
  `cmd::insert_picture_at(&mut f.compose, at, &cid)` (at the editor's cursor — the form owns
  its `Editor`, so take `ed.sel.start`). `paste_formatted` (~439) for the signature editor
  uses `pasted_html_with_pictures`. The web/`pick_later` path keeps working: the form must
  accept a picture on a later frame too (same as the message).
- Save (`dialogs.rs` ~1704–1713): send `styles` as JSON (`serde_json::to_value(&styles)`) and
  the pictures the text still uses (`rich::pictures(&text, &styles)` → from `f.compose.inline`,
  base64) in the `account.setSignature` params. Drop the `html` param.
- Hint text: *"Paste one from Outlook or Word and it keeps its look; use Pictures for a logo
  (Outlook's clipboard doesn't carry its images)."* — through `tl!`, in every catalog in
  `crates/ui-egui/src/i18n/`.

### Tests
- `crates/engine/src/tests.rs`: `a_signature_with_a_logo_goes_into_every_message` (set with a
  1×1 PNG; `mail.new`, `mail.reply`, `compose.insertSignature` each put a `PICTURE` char
  styled with the cid and one `InlinePicture` in `inline`; `compose_draft` → `mime::build`
  has a `Content-ID` part and the HTML has `cid:`); `a_signature_keeps_alignment_lists_and_underline`
  (round trip through `account.setSignature` + `signature_rich` is identity on `(text,
  styles)`); `an_old_signature_html_still_reads` (config with only `signature_html`);
  `signature_pictures_are_capped` (9 pictures → the 9th refused with `Params`; a 3 MB one
  refused); junk `styles`/`data` survive.
- `crates/mime`: `pasted_html_with_pictures_keeps_data_uris`.
- `crates/ui-egui/src/tests.rs`: the Signature dialog frame with the format bar renders; the
  i18n test stays green.

### Docs
- `docs/gaps.md`: a line. `README.md` feature table: "signatures with pictures" if signatures
  are mentioned there.

---

## Order of work and commits

1. **Re-auth** (net: oauth, graph, worker; app: keychain key) — one commit, tests green,
   `cargo xtask ci`.
2. **Download** — commit A (FetchBody on the second connection + pausable prefetch + the
   timing log), then commit B+C (one connection per session, retry cap), then D (30 s poll).
3. **Forward** — `mime` first (quoted_html, from_rich pictures, tests), then engine
   (`Quoted`, `bodies`), then UI frame test.
4. **Signature** — model + engine (storage, migration, pictures) first, then the UI (format
   bar, pictures into the dialog).

After each: `docs/gaps.md` line, `cargo xtask ci`, commit, `cargo xtask wasm` (the `mime`,
`model`, `engine` and `ui-egui` changes must build for wasm32 — no clocks, no threads, no
`SystemTime` outside `net`).
