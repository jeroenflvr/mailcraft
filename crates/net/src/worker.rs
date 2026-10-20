//! Per-account workers.
//!
//! [`Pool`] owns one thread per account. Each thread keeps an IMAP connection, runs the
//! engine's jobs in order, replays changes made offline, syncs every folder on connect and
//! periodically, and otherwise IDLEs on the Inbox so new mail shows up at once. Failures never
//! take the app down: a broken connection is retried with exponential back-off, a refused
//! password stops retrying until the user enters a new one.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mailcraft_model::job::Secret;
use mailcraft_model::{AccountConfig, Connections, Event, Flag, Folder, FolderRole, Job, MessageKey, Op, SyncStatus, sort_folders};
use mailcraft_store::{OutboxItem, Store};

use crate::imap::{Client, uid_set};
use crate::jmap;
use crate::stream::Stream;
use crate::sync::{SyncOptions, fetch_body, sync_folder};
use crate::{NetError, Result, smtp};

/// Where passwords live. The desktop app backs this with the system keychain.
pub trait Credentials: Send + Sync {
    fn get(&self, account: &AccountConfig) -> Option<String>;
    fn set(&self, account: &AccountConfig, password: &str) -> std::result::Result<(), String>;
    fn delete(&self, account: &AccountConfig);
}

/// Passwords held in memory only (tests, the CLI, and when no keychain is available).
#[derive(Default)]
pub struct MemoryCredentials(Mutex<HashMap<String, String>>);

impl Credentials for MemoryCredentials {
    fn get(&self, account: &AccountConfig) -> Option<String> {
        self.0.lock().ok().and_then(|m| m.get(&account.id).cloned())
    }
    fn set(&self, account: &AccountConfig, password: &str) -> std::result::Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.insert(account.id.clone(), password.to_string());
        Ok(())
    }
    fn delete(&self, account: &AccountConfig) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(&account.id);
        }
    }
}

/// Sends events and wakes the UI.
#[derive(Clone)]
struct Sink {
    tx: Sender<Event>,
    wake: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl Sink {
    fn emit(&self, e: Event) {
        let _ = self.tx.send(e);
        if let Ok(w) = self.wake.lock()
            && let Some(f) = w.as_ref()
        {
            f();
        }
    }
}

struct Handle {
    tx: Sender<Job>,
    thread: Option<JoinHandle<()>>,
    /// The account's second connection, for work the user sees happen (sending, automatic
    /// replies): it never waits behind a sync. `None` where it can't be used (POP, JMAP sends).
    quick: Option<Sender<Job>>,
    quick_thread: Option<JoinHandle<()>>,
    /// The account's changes still to make on the server, shared with its main connection.
    ops: Pending,
    /// Set when the account's connections are let go: they stop at the next chance (a Graph
    /// wait ends at once) rather than finishing a sync.
    stop: Arc<AtomicBool>,
    /// Which start of the account this is, for the log.
    generation: u64,
}

/// Changes still to make on the server, oldest first (RFC 4549's pending actions), kept in
/// `ops.json`. The pool adds to them as a change is made, and saves them then, so a change
/// survives quitting while the connection is busy with a sync; the connection works through
/// them.
type Pending = Arc<Mutex<Vec<Op>>>;

fn pending(p: &Pending) -> std::sync::MutexGuard<'_, Vec<Op>> {
    p.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What an account signs in with, shared by its connections: the password or refresh token, and
/// the current access token (refreshed by one, used by both).
struct Auth {
    secret: String,
    access: Option<crate::oauth::Tokens>,
}

impl Auth {
    fn shared(secret: impl Into<String>) -> Arc<Mutex<Auth>> {
        Arc::new(Mutex::new(Auth { secret: secret.into(), access: None }))
    }
}

/// Queue a job, unless it's a folder to read that is waiting already (a few messages sent in a
/// row each ask for Sent Items: once is enough).
fn push_job(queue: &mut VecDeque<Job>, j: Job) {
    if matches!(j, Job::SyncFolder { .. }) && queue.contains(&j) {
        return;
    }
    queue.push_back(j);
}

/// Whether a job goes on the account's second connection: sending (not over JMAP, whose filing
/// needs the main connection's mailbox ids) and automatic replies.
fn quick_job(j: &Job, acc: &AccountConfig) -> bool {
    match j {
        Job::Send { .. } => acc.pop.is_none() && acc.jmap.is_none(),
        Job::GetVacation { .. } | Job::SetVacation { .. } => acc.pop.is_none(),
        _ => false,
    }
}

/// Runs jobs for every account.
pub struct Pool {
    store: Store,
    creds: Arc<dyn Credentials>,
    sink: Sink,
    rx: Receiver<Event>,
    workers: HashMap<String, Handle>,
    configs: HashMap<String, AccountConfig>,
    pub opts: SyncOptions,
    /// Where ManageSieve listens on IMAP hosts (automatic replies); tests change it.
    pub sieve_port: u16,
    /// Picture downloads, on their own thread so they never wait behind a sync.
    fetcher: Option<Sender<String>>,
    /// Working offline: no account connects until it's turned off.
    offline: bool,
    /// How accounts use their servers (requests at once, a second connection, how often).
    pub connections: Connections,
}

/// Workers started so far, numbering them in the log.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Drafts that couldn't be saved to the server (offline, or it failed), kept with the account
/// until it connects again.
const PENDING_DRAFTS: &str = "drafts-pending.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct PendingDraft {
    folder: String,
    #[serde(default)]
    replaces: Option<mailcraft_model::MessageKey>,
    raw: Vec<u8>,
}

/// Keep a draft on this computer for the next connection.
fn keep_draft(store: &Store, account: &str, folder: &str, replaces: Option<mailcraft_model::MessageKey>, raw: Vec<u8>) -> Result<()> {
    let mut list: Vec<PendingDraft> = store.load_account_doc(account, PENDING_DRAFTS).unwrap_or_default();
    // A newer version of the same draft replaces the one kept.
    if let Some(r) = &replaces {
        list.retain(|d| d.replaces.as_ref() != Some(r));
    }
    list.push(PendingDraft { folder: folder.to_string(), replaces, raw });
    store.save_account_doc(account, PENDING_DRAFTS, &list).map_err(NetError::from)
}

pub use mailcraft_model::MICROSOFT_NO_PASSWORDS;

/// What a mailbox Microsoft won't open over IMAP needs: IMAP switched on (Outlook.com's own
/// setting; an organisation's administrator for work accounts), the right account picked in the
/// browser, or Microsoft Graph, which doesn't use IMAP.
fn microsoft_imap_closed(email: &str, signed_in_as: Option<&str>) -> String {
    let who = signed_in_as.map(|o| format!(" (the browser signed in as {o})")).unwrap_or_default();
    let fix = if mailcraft_model::microsoft_consumer(email) {
        "Turn on IMAP on Outlook.com (Settings › Mail › Forwarding and IMAP), or connect the account through Microsoft Graph instead"
    } else {
        "Your organisation may have turned IMAP off: connect the account through Microsoft Graph instead, or ask its administrator"
    };
    format!("{}{who}. {fix}.", mailcraft_model::MICROSOFT_IMAP_CLOSED)
}

pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)).unwrap_or(0)
}

impl Pool {
    pub fn new(store: Store, creds: Arc<dyn Credentials>) -> Pool {
        let (tx, rx) = mpsc::channel();
        Pool {
            store,
            creds,
            sink: Sink { tx, wake: Arc::new(Mutex::new(None)) },
            rx,
            workers: HashMap::new(),
            configs: HashMap::new(),
            opts: SyncOptions::default(),
            sieve_port: crate::sieve::PORT,
            fetcher: None,
            offline: false,
            connections: Connections::default(),
        }
    }

    /// Browser sign-in for an OAuth account, on its own thread: the page opens through the UI
    /// (`Event::OpenUrl`), the refresh token goes to the keychain, then the account starts.
    fn sign_in(&mut self, account: AccountConfig) {
        let Some(p) = crate::oauth::provider_for_account(&account) else {
            let error = format!("signing in with {} isn't set up in this copy of MailCraft", account.oauth.as_deref().unwrap_or("that provider"));
            return self.sink.emit(Event::Status { account: account.id, status: SyncStatus::NeedsPassword { error } });
        };
        let (sink, creds) = (self.sink.clone(), self.creds.clone());
        let spawned = std::thread::Builder::new().name("mail-sign-in".into()).spawn(move || {
            let mut open = |url: &str| -> std::result::Result<(), String> {
                sink.emit(Event::OpenUrl { url: url.to_string() });
                Ok(())
            };
            match crate::oauth::authorize(&p, &account.email, &mut open, Duration::from_secs(5 * 60)) {
                Ok(t) => match t.refresh {
                    Some(r) => match creds.set(&account, &r) {
                        Ok(()) => sink.emit(Event::SignedIn { account: account.id, user: t.user }),
                        Err(e) => sink.emit(Event::Status {
                            account: account.id,
                            status: SyncStatus::NeedsPassword { error: format!("the sign-in couldn't be kept in the keychain: {e}") },
                        }),
                    },
                    None => sink.emit(Event::Status {
                        account: account.id,
                        status: SyncStatus::NeedsPassword { error: format!("{} gave no lasting sign-in; try again", p.name) },
                    }),
                },
                Err(e) => sink.emit(Event::Status { account: account.id, status: SyncStatus::NeedsPassword { error: e.to_string() } }),
            }
        });
        if let Err(e) = spawned {
            log::warn!("sign-in: {e}");
        }
    }

    fn fetch_url(&mut self, url: String) {
        if self.fetcher.is_none() {
            let (tx, rx) = mpsc::channel::<String>();
            let sink = self.sink.clone();
            let spawned = std::thread::Builder::new().name("mail-pictures".into()).spawn(move || {
                for url in rx {
                    let r = crate::http::get(&url, MAX_PICTURE);
                    let e = match r {
                        Ok(resp) if resp.ok() => Event::Fetched { url, data: Some(resp.body), error: None },
                        Ok(resp) => Event::Fetched { url, data: None, error: Some(format!("HTTP {}", resp.status)) },
                        Err(e) => Event::Fetched { url, data: None, error: Some(e.to_string()) },
                    };
                    sink.emit(e);
                }
            });
            match spawned {
                Ok(_) => self.fetcher = Some(tx),
                Err(e) => return self.sink.emit(Event::Fetched { url, data: None, error: Some(e.to_string()) }),
            }
        }
        if let Some(tx) = &self.fetcher {
            let _ = tx.send(url);
        }
    }

    /// Export to a data file on a thread of its own.
    fn export_pst(&mut self, account: String, path: String, name: String) {
        let sink = self.sink.clone();
        let store = self.store.clone();
        let spawned = std::thread::Builder::new().name("mail-export".into()).spawn(move || {
            let mut progress = |messages| sink.emit(Event::Exported { account: account.clone(), messages, skipped: 0, done: false, error: None });
            let event = match crate::pst_export::export(&store, &account, &name, std::path::Path::new(&path), &mut progress) {
                Ok(done) => Event::Exported { account, messages: done.messages, skipped: done.skipped, done: true, error: None },
                Err(e) => Event::Exported { account, messages: 0, skipped: 0, done: true, error: Some(e) },
            };
            sink.emit(event);
        });
        if let Err(e) = spawned {
            self.sink.emit(Event::Notice { account: String::new(), message: format!("export: {e}") });
        }
    }

    /// Import a data file on a thread of its own: an Outlook one (`.pst`), or MailCraft's own
    /// (`.mailcraft`, copied in as it is).
    fn import_pst(&mut self, account: String, path: String) {
        let sink = self.sink.clone();
        let store = self.store.clone();
        let spawned = std::thread::Builder::new().name("mail-import".into()).spawn(move || {
            if path.to_ascii_lowercase().ends_with(".mailcraft") {
                match store.import_data_file(&account, std::path::Path::new(&path)) {
                    Ok(messages) => {
                        let folders = store.load_folders(&account).unwrap_or_default();
                        let paths: Vec<String> = folders.iter().map(|f| f.path.clone()).collect();
                        sink.emit(Event::Folders { account: account.clone(), folders });
                        for folder in paths {
                            sink.emit(Event::FolderChanged { account: account.clone(), folder });
                        }
                        sink.emit(Event::Imported { account, messages, skipped: 0, done: true, error: None });
                    }
                    Err(e) => sink.emit(Event::Imported { account, messages: 0, skipped: 0, done: true, error: Some(e.to_string()) }),
                }
                return;
            }
            let mut progress = |messages| sink.emit(Event::Imported { account: account.clone(), messages, skipped: 0, done: false, error: None });
            let result = crate::pst::import(std::path::Path::new(&path), &store, &account, &mut progress);
            match result {
                Ok(done) => {
                    let paths: Vec<String> = done.folders.iter().map(|f| f.path.clone()).collect();
                    sink.emit(Event::Folders { account: account.clone(), folders: done.folders });
                    for folder in paths {
                        sink.emit(Event::FolderChanged { account: account.clone(), folder });
                    }
                    sink.emit(Event::Imported { account, messages: done.messages, skipped: done.skipped, done: true, error: None });
                }
                Err(e) => sink.emit(Event::Imported { account, messages: 0, skipped: 0, done: true, error: Some(e) }),
            }
        });
        if let Err(e) = spawned {
            self.sink.emit(Event::Notice { account: String::new(), message: format!("import: {e}") });
        }
    }

    /// Called (from worker threads) after every event, e.g. to request a UI repaint.
    pub fn set_waker(&self, f: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut w) = self.sink.wake.lock() {
            *w = Some(f);
        }
    }

    pub fn submit(&mut self, job: Job) {
        match job {
            Job::FetchUrl { url } => self.fetch_url(url),
            Job::SignIn { account } => self.sign_in(account),
            Job::Discover { email } => {
                let sink = self.sink.clone();
                let spawned = std::thread::Builder::new().name("mail-discover".into()).spawn(move || {
                    let found = crate::autoconfig::discover(&email);
                    let (config, source) = match found {
                        Some(f) => (Some(Box::new(f.config)), f.source),
                        None => (None, String::new()),
                    };
                    sink.emit(Event::Discovered { email, config, source });
                });
                if let Err(e) = spawned {
                    log::warn!("autoconfiguration: {e}");
                }
            }
            Job::ImportPst { account, path } => self.import_pst(account, path),
            Job::ExportPst { account, path, name } => self.export_pst(account, path, name),
            Job::SetSync { messages, offline, connections } => {
                let opts =
                    SyncOptions { window: if messages == 0 { usize::MAX } else { messages as usize }, prefetch: offline as usize, ..self.opts };
                let connections = connections.clamped();
                if opts.window != self.opts.window || opts.prefetch != self.opts.prefetch || connections != self.connections {
                    log::info!(
                        "sync settings: {messages} messages a folder, {offline} downloaded; {} requests at once, second connection {}, new mail every {} min, every folder every {} min",
                        connections.requests,
                        if connections.second { "on" } else { "off" },
                        connections.check_minutes,
                        connections.sync_all_minutes
                    );
                    self.opts = opts;
                    self.connections = connections;
                    // Connected accounts start again with it (a rare change, from Options).
                    let running: Vec<AccountConfig> = self.workers.keys().filter_map(|id| self.configs.get(id).cloned()).collect();
                    for c in running {
                        self.start(c, None);
                    }
                }
            }
            Job::WorkOffline { on } => {
                self.offline = on;
                if on {
                    let ids: Vec<String> = self.workers.keys().cloned().collect();
                    for id in ids {
                        self.stop(&id);
                        self.sink.emit(Event::Status { account: id, status: SyncStatus::Offline });
                    }
                } else {
                    let configs: Vec<AccountConfig> = self.configs.values().cloned().collect();
                    for c in configs {
                        self.start(c, None);
                    }
                }
            }
            Job::Start { account, password } if self.offline => {
                // Working offline: remembered (and its password kept) for when that's turned off.
                if let Some(p) = &password
                    && let Err(e) = self.creds.set(&account, &p.0)
                {
                    self.sink.emit(Event::Notice {
                        account: account.id.clone(),
                        message: format!("the password could not be saved in the keychain: {e}"),
                    });
                }
                self.sink.emit(Event::Status { account: account.id.clone(), status: SyncStatus::Offline });
                self.configs.insert(account.id.clone(), account);
            }
            Job::Start { account, password } => self.start(account, password),
            Job::Remove { account } => {
                self.stop(&account);
                if let Some(c) = self.configs.remove(&account) {
                    self.creds.delete(&c);
                }
            }
            Job::FetchBody { key } if !self.workers.contains_key(&key.account) => {
                // Offline or not started: the cache may still have it.
                match self.store.read_body(&key) {
                    Some(raw) => self.sink.emit(Event::Body { key, raw }),
                    None => self.sink.emit(Event::BodyFailed { key, error: "not downloaded yet, and the account is offline".into() }),
                }
            }
            Job::Send { account, id, raw, from, recipients, sent_folder } if !self.workers.contains_key(&account) => {
                // Not connected: straight into the Outbox.
                let item = OutboxItem { id, from, recipients, sent_folder, queued: now(), attempts: 0, error: Some("offline".into()), not_before: 0 };
                match self.store.outbox_add(&account, &item, &raw) {
                    Ok(()) => self.sink.emit(Event::Queued { account, id, error: "offline".into() }),
                    Err(e) => self.sink.emit(Event::SendFailed { account, id, error: e.to_string() }),
                }
            }
            // Offline: changes wait in the cache for the next sync.
            Job::SyncDav { account } if !self.workers.contains_key(&account) => {}
            // Working offline: nothing connects, by choice.
            Job::SyncAll { account } | Job::SyncFolder { account, .. } | Job::FlushOutbox { account }
                if self.offline && !self.workers.contains_key(&account) => {}
            Job::SaveDraft { account, raw, folder, replaces } if !self.workers.contains_key(&account) => {
                match keep_draft(&self.store, &account, &folder, replaces, raw) {
                    Ok(()) => self.sink.emit(Event::Notice {
                        account,
                        message: "the draft is kept on this computer; it goes to Drafts when you're back online".into(),
                    }),
                    Err(e) => self.sink.emit(Event::Notice { account, message: format!("the draft couldn't be kept: {e}") }),
                }
            }
            Job::GetVacation { account } | Job::SetVacation { account, .. } if !self.workers.contains_key(&account) => {
                self.sink.emit(Event::Vacation { account, vacation: None, error: Some("the account is offline".into()) });
            }
            Job::Apply { account, op } if !self.workers.contains_key(&account) => {
                // Offline: change the cache now and keep the change for the next connection.
                match self.store.apply_op(&account, &op) {
                    Ok(changed) => {
                        if op.is_folder_op() {
                            let folders = self.store.load_folders(&account).unwrap_or_default();
                            self.sink.emit(Event::Folders { account: account.clone(), folders });
                        }
                        let mut ops = self.store.load_ops(&account).unwrap_or_default();
                        ops.push(op);
                        if let Err(e) = self.store.save_ops(&account, &ops) {
                            self.sink.emit(Event::Notice { account: account.clone(), message: e.to_string() });
                        }
                        for folder in changed {
                            self.sink.emit(Event::FolderChanged { account: account.clone(), folder });
                        }
                    }
                    Err(e) => self.sink.emit(Event::Notice { account, message: e.to_string() }),
                }
            }
            other => {
                let id = other.account().to_string();
                let quick = self.configs.get(&id).is_some_and(|c| quick_job(&other, c));
                let pop = self.configs.get(&id).is_some_and(|c| c.pop.is_some());
                match self.workers.get(&id) {
                    Some(h) => {
                        // Kept now, not when the connection gets to it (a sync can take a minute):
                        // quitting meanwhile mustn't lose it.
                        if let Job::Apply { op, .. } = &other {
                            let mut ops = pending(&h.ops);
                            ops.push(op.clone());
                            if let Err(e) = self.store.save_ops(&id, &ops) {
                                self.sink.emit(Event::Notice { account: id.clone(), message: e.to_string() });
                            }
                            let waiting = ops.len();
                            drop(ops);
                            // And in the cache at once: reading a folder from it meanwhile (or
                            // after quitting) shows the change, not the server's state before it.
                            // POP moves are the connection's to make (they file a copy).
                            if op.is_message_op()
                                && !pop
                                && let Err(e) = self.store.apply_op(&id, op)
                            {
                                log::warn!("{id}: the change couldn't be kept in the cache: {e}");
                            }
                            log::info!("{id}: change waiting for the server: {} ({waiting} waiting)", op.describe());
                        }
                        // Sending and automatic replies go on the second connection; if it has
                        // stopped, the main one does it (after its sync).
                        let other = match h.quick.as_ref().filter(|_| quick) {
                            Some(q) => match q.send(other) {
                                Ok(()) => return,
                                Err(mpsc::SendError(j)) => j,
                            },
                            None => other,
                        };
                        if h.tx.send(other).is_err() {
                            self.sink.emit(Event::Notice {
                                account: id,
                                message: "the account's connection stopped; restart it from the account menu".into(),
                            });
                        }
                    }
                    None => self.sink.emit(Event::Notice { account: id, message: "the account is not connected".into() }),
                }
            }
        }
    }

    /// Events reported since the last call.
    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Block until an event arrives or `timeout` passes (for the CLI).
    pub fn wait(&self, timeout: Duration) -> Option<Event> {
        self.rx.recv_timeout(timeout).ok()
    }

    fn start(&mut self, account: AccountConfig, password: Option<Secret>) {
        let previous = self.release(&account.id);
        if let Some(p) = &password
            && let Err(e) = self.creds.set(&account, &p.0)
        {
            self.sink.emit(Event::Notice { account: account.id.clone(), message: format!("the password could not be saved in the keychain: {e}") });
        }
        let pw = password.map(|p| p.0).or_else(|| self.creds.get(&account));
        let Some(pw) = pw else {
            self.sink.emit(Event::Status { account: account.id.clone(), status: SyncStatus::NeedsPassword { error: String::new() } });
            Self::forget(previous);
            return;
        };
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let auth = Auth::shared(pw);
        let mut w = Worker::new(account.clone(), auth.clone(), self.store.clone(), self.sink.clone(), self.opts);
        w.creds = Some(self.creds.clone());
        w.sieve_port = self.sieve_port;
        w.connections = self.connections;
        w.stop = stop.clone();
        w.generation = generation;
        // The second connection: quiet (the account's status is the main one's), connecting
        // only when there's something to do. Turned off, the main one sends too.
        let (quick, quick_thread) = if self.connections.second {
            let (qtx, qrx) = mpsc::channel();
            let mut q = Worker::new(account.clone(), auth, self.store.clone(), self.sink.clone(), self.opts);
            q.creds = Some(self.creds.clone());
            q.sieve_port = self.sieve_port;
            q.main = Some(tx.clone());
            q.connections = self.connections;
            q.stop = stop.clone();
            q.generation = generation;
            let t = std::thread::Builder::new().name(format!("mail-{}-quick", account.id)).spawn(move || q.run_quick(qrx)).ok();
            (t.is_some().then_some(qtx), t)
        } else {
            (None, None)
        };
        // The connections this one replaces finish first: two at once would save older state
        // over newer.
        w.previous = previous;
        let name = format!("mail-{}", account.id);
        let ops = w.ops.clone();
        match std::thread::Builder::new().name(name).spawn(move || w.run(rx)) {
            Ok(thread) => {
                self.workers.insert(account.id.clone(), Handle { tx, thread: Some(thread), quick, quick_thread, ops, stop, generation });
            }
            Err(e) => self.sink.emit(Event::Status { account: account.id.clone(), status: SyncStatus::Error { error: e.to_string() } }),
        }
        self.configs.insert(account.id.clone(), account);
    }

    /// Tell an account's connections to stop, and hand back their threads.
    fn release(&mut self, account: &str) -> Vec<JoinHandle<()>> {
        let Some(mut h) = self.workers.remove(account) else { return Vec::new() };
        log::info!("{account}: letting go of connection {}", h.generation);
        h.stop.store(true, Ordering::Relaxed);
        drop(h.tx);
        drop(h.quick.take());
        [h.thread.take(), h.quick_thread.take()].into_iter().flatten().collect()
    }

    /// The threads notice within half a second (IDLE slices, Graph waits); don't block the UI
    /// waiting for a slow network call to finish.
    fn forget(threads: Vec<JoinHandle<()>>) {
        for t in threads {
            std::thread::spawn(move || {
                let _ = t.join();
            });
        }
    }

    fn stop(&mut self, account: &str) {
        let threads = self.release(account);
        Self::forget(threads);
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<String> = self.workers.keys().cloned().collect();
        for id in ids {
            self.stop(&id);
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Largest picture downloaded for a message.
const MAX_PICTURE: usize = 8 << 20;

const IDLE_FOR: Duration = Duration::from_secs(10 * 60);
/// How long a new connection waits for the one it replaces to finish.
const REPLACED_WAIT: Duration = Duration::from_secs(30);
/// A folder sync that took longer than this is logged even when nothing changed.
const SLOW_SYNC: Duration = Duration::from_secs(5);
/// When Sent Items is read again after Graph or Exchange took a message (its copy is filed a
/// moment later), and once more in case that was too soon.
const SENT_SOON: Duration = Duration::from_secs(3);
const SENT_LATER: Duration = Duration::from_secs(15);
/// An account's second connection lets go after this long with nothing to do.
const QUICK_IDLE: Duration = Duration::from_secs(5 * 60);

/// The connection to an account's server.
enum Conn {
    Imap(Client<Stream>),
    Jmap(jmap::Client),
    Graph(crate::graph::Client),
    Ews(Box<crate::ews::Client>),
    /// POP3: signed in once to check the password; every download is a session of its own.
    Pop,
}

struct Worker {
    acc: AccountConfig,
    auth: Arc<Mutex<Auth>>,
    /// Set on an account's second connection: the main one's queue, for what this one leaves to
    /// it (a message that can't go yet, Sent Items to look at).
    main: Option<Sender<Job>>,
    store: Store,
    sink: Sink,
    opts: SyncOptions,
    conn: Option<Conn>,
    /// JMAP and Graph accounts: message numbers and mailbox ids.
    jstate: jmap::State,
    folders: Vec<Folder>,
    ops: Pending,
    queue: VecDeque<Job>,
    backoff: Duration,
    next_attempt: Instant,
    last_full: Option<Instant>,
    /// Not connecting again until asked (Send / Receive, a new start): the sign-in was refused,
    /// or the server said no to the account itself.
    auth_failed: bool,
    closed: bool,
    /// Where a rotated OAuth refresh token is saved.
    creds: Option<Arc<dyn Credentials>>,
    /// The ManageSieve port (4190; tests use their own).
    sieve_port: u16,
    /// The JMAP server can't push: poll instead.
    no_push: bool,
    /// The last push wait ended in an Inbox sync (so the next one needn't check first).
    pushed: bool,
    /// Folders to look at again soon: Graph and Exchange file a sent message's copy a moment
    /// after taking it, so Sent Items is read again then rather than at the next poll.
    soon: Vec<(Instant, String)>,
    /// Requests at once, a second connection, how often to check.
    connections: Connections,
    /// Set by the pool when this connection is let go.
    stop: Arc<AtomicBool>,
    /// Which start of the account this is, for the log.
    generation: u64,
    /// The connections this one replaces, waited for before connecting.
    previous: Vec<JoinHandle<()>>,
    /// The last status logged (each change is logged once).
    logged: std::cell::Cell<Option<std::mem::Discriminant<SyncStatus>>>,
    /// The wait before trying again after the folder list couldn't be read (not reset by
    /// connecting, only by a whole sync going through).
    relist_backoff: Duration,
}

impl Worker {
    fn new(acc: AccountConfig, auth: Arc<Mutex<Auth>>, store: Store, sink: Sink, opts: SyncOptions) -> Worker {
        let ops = Arc::new(Mutex::new(store.load_ops(&acc.id).unwrap_or_default()));
        let folders = store.load_folders(&acc.id).unwrap_or_default();
        let jstate = if acc.is_api() { jmap::State::load(&store, &acc.id) } else { jmap::State::default() };
        Worker {
            acc,
            auth,
            main: None,
            store,
            sink,
            opts,
            conn: None,
            jstate,
            folders,
            ops,
            queue: VecDeque::new(),
            backoff: Duration::from_secs(5),
            next_attempt: Instant::now(),
            last_full: None,
            auth_failed: false,
            closed: false,
            creds: None,
            sieve_port: crate::sieve::PORT,
            no_push: false,
            pushed: false,
            soon: Vec::new(),
            connections: Connections::default(),
            stop: Arc::new(AtomicBool::new(false)),
            generation: 0,
            previous: Vec::new(),
            logged: std::cell::Cell::new(None),
            relist_backoff: Duration::from_secs(5),
        }
    }

    /// Minutes between checks for new mail where the server can't say at once.
    fn poll_every(&self) -> Duration {
        Duration::from_secs(u64::from(self.connections.check_minutes.max(1)) * 60)
    }

    /// Minutes between reads of every folder.
    fn full_sync_every(&self) -> Duration {
        Duration::from_secs(u64::from(self.connections.sync_all_minutes.max(1)) * 60)
    }

    /// The pool let go of this connection.
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// What to sign in with: the password, or for OAuth accounts a current access token
    /// (refreshed from the stored refresh token when needed).
    fn secret(&mut self) -> Result<String> {
        // Held while refreshing: the account's other connection waits for the new token rather
        // than refreshing with the same old one.
        let mut auth = self.auth.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(id) = self.acc.oauth.clone() else { return Ok(auth.secret.clone()) };
        if let Some(t) = &auth.access
            && t.expires_at > Instant::now()
        {
            return Ok(t.access.clone());
        }
        let p = crate::oauth::provider_for_account(&self.acc)
            .ok_or_else(|| NetError::Auth(format!("signing in with {id} isn't set up in this copy of MailCraft")))?;
        let t = crate::oauth::refresh(&p, &auth.secret)?;
        // Some providers hand out a new refresh token each time: keep the newest.
        if let Some(r) = t.refresh.clone().filter(|r| *r != auth.secret) {
            auth.secret = r.clone();
            if let Some(c) = &self.creds
                && let Err(e) = c.set(&self.acc, &r)
            {
                self.sink
                    .emit(Event::Notice { account: self.acc.id.clone(), message: format!("the new sign-in couldn't be saved in the keychain: {e}") });
            }
        }
        let access = t.access.clone();
        auth.access = Some(t);
        Ok(access)
    }

    /// What Microsoft's sign-in granted (its scopes), when known.
    fn granted(&self) -> String {
        self.auth.lock().unwrap_or_else(std::sync::PoisonError::into_inner).access.as_ref().map(|t| t.granted.clone()).unwrap_or_default()
    }

    fn id(&self) -> String {
        self.acc.id.clone()
    }

    fn status(&self, status: SyncStatus) {
        // The second connection's comings and goings aren't the account's; nor are those of a
        // connection let go (the pool said Offline, or a new connection has the account).
        if self.main.is_none() && !self.stopping() {
            let kind = std::mem::discriminant(&status);
            let changed = self.logged.replace(Some(kind)) != Some(kind);
            let id = &self.acc.id;
            match &status {
                SyncStatus::Error { error } => log::warn!("{id}: {error}"),
                SyncStatus::NeedsPassword { error } => log::warn!("{id}: needs signing in again: {error}"),
                // Each folder has a line of its own when it's done.
                SyncStatus::Syncing { folder } => log::debug!("{id}: reading {folder}"),
                SyncStatus::Online { .. } if changed => log::info!("{id}: up to date"),
                SyncStatus::Online { .. } => {}
                s if changed => log::info!("{id}: {s:?}"),
                _ => {}
            }
            self.sink.emit(Event::Status { account: self.id(), status });
        }
    }

    fn notice(&self, message: impl Into<String>) {
        self.sink.emit(Event::Notice { account: self.id(), message: message.into() });
    }

    fn drain(&mut self, rx: &Receiver<Job>) {
        loop {
            match rx.try_recv() {
                Ok(j) => push_job(&mut self.queue, j),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.closed = true;
                    return;
                }
            }
        }
    }

    fn run(mut self, rx: Receiver<Job>) {
        self.status(SyncStatus::Connecting);
        self.wait_for_previous();
        log::info!("{}: connection {} started", self.acc.id, self.generation);
        // Changes still waiting for the server, kept over what the cache has (it may predate
        // them: an older version, or quitting at the wrong moment).
        for folder in self.keep_pending(None) {
            self.sink.emit(Event::FolderChanged { account: self.id(), folder });
        }
        while !self.closed && !self.stopping() {
            self.drain(&rx);
            if self.closed || self.stopping() {
                break;
            }
            if let Some(job) = self.queue.pop_front() {
                self.handle(job);
                continue;
            }
            if self.conn.is_none() {
                if self.auth_failed {
                    match rx.recv() {
                        Ok(j) => push_job(&mut self.queue, j),
                        Err(_) => break,
                    }
                    continue;
                }
                let wait = self.next_attempt.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    match rx.recv_timeout(wait) {
                        Ok(j) => push_job(&mut self.queue, j),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    continue;
                }
                self.connect_and_sync();
                continue;
            }
            if self.last_full.is_none_or(|t| t.elapsed() >= self.full_sync_every()) {
                self.sync_all(&rx);
                continue;
            }
            self.wait_for_mail(&rx);
        }
        if let Some(Conn::Imap(c)) = self.conn.as_mut() {
            c.logout();
        }
        log::info!("{}: connection {} stopped", self.acc.id, self.generation);
    }

    /// Wait (a while, not for good) for the connections this one replaces to finish.
    fn wait_for_previous(&mut self) {
        let end = Instant::now() + REPLACED_WAIT;
        for t in std::mem::take(&mut self.previous) {
            while !t.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(50));
            }
            if t.is_finished() {
                let _ = t.join();
            } else {
                log::warn!("{}: the connection replaced is still busy after {} s; starting anyway", self.acc.id, REPLACED_WAIT.as_secs());
            }
        }
    }

    /// Make the changes still waiting for the server in the cache again (all of them, or those
    /// touching `folder`): a sync reads the server's state, which doesn't have them yet
    /// (RFC 4549). The folders changed.
    fn keep_pending(&self, folder: Option<&str>) -> Vec<String> {
        if self.acc.pop.is_some() {
            return Vec::new();
        }
        let ops: Vec<Op> = pending(&self.ops)
            .iter()
            .filter(|op| op.is_message_op() && folder.is_none_or(|f| op.keys().iter().any(|k| k.folder == f)))
            .cloned()
            .collect();
        let mut changed = Vec::new();
        for op in &ops {
            match self.store.apply_op(&self.acc.id, op) {
                Ok(c) => changed.extend(c),
                Err(e) => log::warn!("{}: a waiting change couldn't be kept in the cache: {e}", self.acc.id),
            }
        }
        if !ops.is_empty() {
            log::debug!("{}: {} waiting change(s) kept over what the server said", self.acc.id, ops.len());
        }
        changed.sort();
        changed.dedup();
        changed
    }

    /// The account's second connection: sends mail and reads or changes the automatic replies,
    /// whatever the main connection is doing. It connects when there's something to do, and lets
    /// go of an idle connection after a while.
    fn run_quick(mut self, rx: Receiver<Job>) {
        loop {
            let job = match rx.recv_timeout(QUICK_IDLE) {
                Ok(j) => j,
                Err(RecvTimeoutError::Timeout) => {
                    self.let_go();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            match job {
                Job::Send { account, id, raw, from, recipients, sent_folder } => {
                    self.send_quick(Job::Send { account, id, raw, from, recipients, sent_folder });
                }
                j @ (Job::GetVacation { .. } | Job::SetVacation { .. }) => self.handle(j),
                // Anything else belongs to the main connection.
                other => {
                    if let Some(m) = &self.main {
                        let _ = m.send(other);
                    }
                }
            }
        }
        self.let_go();
    }

    /// Close an idle connection (servers drop them anyway).
    fn let_go(&mut self) {
        if let Some(Conn::Imap(c)) = self.conn.as_mut() {
            c.logout();
        }
        self.conn = None;
    }

    /// Send on the second connection. Sent: a copy filed in Sent Items (IMAP), and the main
    /// connection told to look there. Not sent for now (offline, asked to wait): handed to the
    /// main connection, which keeps it in the Outbox; refused: said.
    fn send_quick(&mut self, job: Job) {
        let Job::Send { id, raw, from, recipients, sent_folder, .. } = &job else { return };
        match self.deliver(raw, from, recipients, sent_folder.as_deref()) {
            Ok(()) => {
                log::info!("{}: message {id} sent to {} recipient(s)", self.acc.id, recipients.len());
                self.sink.emit(Event::Sent { account: self.id(), id: *id });
                self.file_sent_quick(raw, sent_folder.clone());
            }
            Err(e) if e.is_retryable() => {
                log::info!("{}: sending on the second connection: {e}; left to the main one", self.acc.id);
                self.conn = None;
                if let Some(m) = &self.main
                    && m.send(job.clone()).is_err()
                {
                    self.sink.emit(Event::SendFailed { account: self.id(), id: *id, error: e.to_string() });
                }
            }
            Err(e) => {
                if matches!(e, NetError::Closed(_) | NetError::Io(_)) {
                    self.conn = None;
                }
                self.sink.emit(Event::SendFailed { account: self.id(), id: *id, error: e.to_string() });
            }
        }
    }

    /// A sent message's copy: Exchange, Graph and JMAP file it themselves; IMAP servers get one
    /// (not Gmail, which files its own). Then the main connection reads Sent Items, now and, where
    /// the server files it a moment later, again then.
    fn file_sent_quick(&mut self, raw: &[u8], sent_folder: Option<String>) {
        let folder = sent_folder.or_else(|| self.folders.iter().find(|f| f.role == FolderRole::Sent).map(|f| f.path.clone()));
        let Some(folder) = folder else { return };
        if !self.acc.is_api() {
            let files_itself = self.acc.smtp.host.ends_with("gmail.com") || self.acc.smtp.host.ends_with("googlemail.com");
            if !files_itself {
                let r = (|| -> Result<()> {
                    if self.conn.is_none() {
                        self.connect()?;
                    }
                    self.client()?.append(&folder, "\\Seen", raw).map(|_| ())
                })();
                if let Err(e) = r {
                    self.conn = None;
                    self.notice(format!("sent, but no copy was saved in Sent Items: {e}"));
                    return;
                }
            }
        }
        let Some(main) = self.main.clone() else { return };
        let job = Job::SyncFolder { account: self.id(), folder: folder.clone() };
        let _ = main.send(job.clone());
        if self.acc.is_api() {
            // Graph and Exchange file the copy a moment after taking the message.
            let _ = std::thread::Builder::new().name("sent-later".into()).spawn(move || {
                for wait in [SENT_SOON, SENT_LATER.saturating_sub(SENT_SOON)] {
                    std::thread::sleep(wait);
                    if main.send(job.clone()).is_err() {
                        return;
                    }
                }
            });
        }
    }

    /// Wait for new mail: IDLE on the Inbox when the server supports it, else poll.
    fn wait_for_mail(&mut self, rx: &Receiver<Job>) {
        let inbox = self.inbox();
        if matches!(self.conn, Some(Conn::Jmap(_))) && !self.no_push && inbox.is_some() && self.wait_for_push(rx) {
            return;
        }
        let idle = matches!(&self.conn, Some(Conn::Imap(c)) if c.has("IDLE"));
        if !idle || inbox.is_none() {
            let next_soon = self.soon.iter().map(|(at, _)| *at).min();
            let every = self.poll_every();
            let wait = next_soon.map_or(every, |at| at.saturating_duration_since(Instant::now()).min(every));
            match rx.recv_timeout(wait) {
                Ok(j) => push_job(&mut self.queue, j),
                Err(RecvTimeoutError::Disconnected) => self.closed = true,
                Err(RecvTimeoutError::Timeout) if next_soon.is_some_and(|at| at <= Instant::now()) => {
                    let now = Instant::now();
                    let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.soon).into_iter().partition(|(at, _)| *at <= now);
                    self.soon = later;
                    let mut folders: Vec<String> = due.into_iter().map(|(_, f)| f).collect();
                    folders.dedup();
                    for f in folders {
                        self.sync_one(&f);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(f) = inbox {
                        self.sync_one(&f);
                    }
                }
            }
            return;
        }
        let Some(inbox) = inbox else { return };
        let result = (|| -> Result<bool> {
            let Some(Conn::Imap(c)) = self.conn.as_mut() else { return Err(NetError::Closed("not connected".into())) };
            if c.selected() != Some(inbox.as_str()) {
                c.select(&inbox, false)?;
            }
            let queue = &mut self.queue;
            let closed = &mut self.closed;
            let stop = self.stop.clone();
            c.idle(IDLE_FOR, || {
                if stop.load(Ordering::Relaxed) {
                    *closed = true;
                }
                loop {
                    match rx.try_recv() {
                        Ok(j) => push_job(queue, j),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            *closed = true;
                            break;
                        }
                    }
                }
                !queue.is_empty() || *closed
            })
        })();
        match result {
            Ok(true) => self.sync_one(&inbox),
            Ok(false) => {}
            Err(e) => self.failed(e),
        }
    }

    /// JMAP push: hold the server's event stream open (as IMAP holds IDLE), checking for jobs
    /// between short reads; a `state` event means something changed. `false` when the server
    /// can't push (the caller polls instead).
    fn wait_for_push(&mut self, rx: &Receiver<Job>) -> bool {
        let Some(Conn::Jmap(c)) = &self.conn else { return false };
        let mut es = match c.events() {
            Ok(Some(es)) => es,
            Ok(None) => {
                self.no_push = true;
                return false;
            }
            Err(e @ NetError::Auth(_)) => {
                self.failed(e);
                return true;
            }
            Err(e) => {
                // Push not working (a proxy, an old server): poll from now on.
                log::info!("{}: no JMAP push: {e}", self.acc.id);
                self.no_push = true;
                return false;
            }
        };
        // Mail that came between the last sync and this stream opening sends no event (unless
        // the server replays it, RFC 8620 §7.3): check once now that nothing will be missed.
        if !std::mem::take(&mut self.pushed)
            && let Some(f) = self.inbox()
        {
            self.sync_one(&f);
        }
        let end = Instant::now() + IDLE_FOR;
        let mut changed = false;
        while Instant::now() < end && !changed {
            self.drain(rx);
            if self.closed || self.stopping() || !self.queue.is_empty() {
                return true;
            }
            match es.next(Duration::from_millis(500)) {
                Ok(Some(e)) => changed = e.event == "state",
                Ok(None) => {}
                Err(e) => {
                    // Dropped: reopened on the next wait.
                    log::info!("{}: JMAP push ended: {e}", self.acc.id);
                    break;
                }
            }
        }
        if changed && let Some(f) = self.inbox() {
            self.sync_one(&f);
            self.pushed = true;
        }
        true
    }

    fn inbox(&self) -> Option<String> {
        self.folders.iter().find(|f| f.role == FolderRole::Inbox).map(|f| f.path.clone())
    }

    /// Note a failure; drop the connection when it is unusable.
    fn failed(&mut self, e: NetError) {
        self.failed_in("", e);
    }

    /// [`Worker::failed`], saying in the log what was being done (a folder's name, "sending"…).
    fn failed_in(&mut self, what: &str, e: NetError) {
        // Let go: whatever failed failed because of that.
        if self.stopping() {
            log::debug!("{}: {what}: stopped: {e}", self.acc.id);
            self.conn = None;
            return;
        }
        let kind = if e.is_fatal() { "connection lost" } else { "refused" };
        if what.is_empty() {
            log::warn!("{}: {kind}: {e}", self.acc.id);
        } else {
            log::warn!("{}: {what}: {kind}: {e}", self.acc.id);
        }
        match e {
            // Microsoft took the token but won't open the mailbox: nothing to retry until changed.
            NetError::Auth(m) | NetError::Bad(m) | NetError::No(m)
                if self.acc.oauth.as_deref() == Some("microsoft") && m.to_ascii_lowercase().contains("authenticated but not connected") =>
            {
                self.conn = None;
                self.auth_failed = true;
                self.status(SyncStatus::Error { error: microsoft_imap_closed(&self.acc.email, self.acc.oauth_user.as_deref()) });
            }
            // Microsoft's servers refuse passwords from mail apps: asking for it again only loops.
            NetError::Auth(m) if mailcraft_model::microsoft_password_account(&self.acc) => {
                self.conn = None;
                self.auth_failed = true;
                self.status(SyncStatus::Error { error: format!("{MICROSOFT_NO_PASSWORDS} ({m})") });
            }
            NetError::Auth(m) => {
                self.conn = None;
                self.auth_failed = true;
                self.status(SyncStatus::NeedsPassword { error: m });
            }
            e if e.is_fatal() => {
                self.conn = None;
                self.next_attempt = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(Duration::from_secs(300));
                self.status(SyncStatus::Error { error: e.to_string() });
            }
            e => self.notice(e.to_string()),
        }
    }

    fn connect(&mut self) -> Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        self.status(SyncStatus::Connecting);
        let secret = self.secret()?;
        let user = if self.acc.username.is_empty() { self.acc.email.clone() } else { self.acc.username.clone() };
        if let Some(url) = &self.acc.graph {
            let c = crate::graph::Client::connect(url, &secret)?.with_pacing(self.connections.requests as usize, self.stop.clone());
            self.conn = Some(Conn::Graph(c));
            self.backoff = Duration::from_secs(5);
            return Ok(());
        }
        if self.acc.pop.is_some() {
            crate::pop::open(&self.acc, &secret)?.quit()?;
            self.conn = Some(Conn::Pop);
            self.backoff = Duration::from_secs(5);
            return Ok(());
        }
        if let Some(url) = &self.acc.ews {
            // `autodiscover`: found the first time, then kept.
            let url = if url == crate::ews::AUTODISCOVER {
                let kept: String = self.store.load_account_doc(&self.acc.id, "ews.json").unwrap_or_default();
                if kept.is_empty() {
                    let found = crate::ews::autodiscover(&self.acc.email, &user, &secret, None)?;
                    let _ = self.store.save_account_doc(&self.acc.id, "ews.json", &found);
                    found
                } else {
                    kept
                }
            } else {
                url.clone()
            };
            let c = crate::ews::Client::connect(&url, &user, &secret, &self.acc.email)?;
            c.set_shared(&self.acc.shared_mailboxes);
            self.conn = Some(Conn::Ews(Box::new(c)));
            self.backoff = Duration::from_secs(5);
            return Ok(());
        }
        if let Some(url) = &self.acc.jmap {
            self.conn = Some(Conn::Jmap(jmap::Client::connect(url, &user, &secret, self.acc.oauth.is_some())?));
            self.backoff = Duration::from_secs(5);
            return Ok(());
        }
        let c = if self.acc.oauth.is_some() {
            self.imap_with_token(&user, &secret)?
        } else {
            let mut c = Client::connect(&self.acc.imap)?;
            c.login(&user, &secret)?;
            c
        };
        self.conn = Some(Conn::Imap(c));
        self.backoff = Duration::from_secs(5);
        Ok(())
    }

    /// An IMAP connection signed in with an OAuth token. Microsoft can take the token and then
    /// refuse the mailbox ("User is authenticated but not connected"): IMAP is switched off for
    /// it (Outlook.com's own setting, or an organisation's), or the browser signed in as another
    /// address. The address it signed in as is tried once; otherwise it's a refusal that says
    /// what to do, not a sign-in to ask for again (that only loops).
    fn imap_with_token(&self, user: &str, token: &str) -> Result<Client<Stream>> {
        let mut c = Client::connect(&self.acc.imap)?;
        let m = match c.login_xoauth2(user, token) {
            Ok(()) => return Ok(c),
            Err(NetError::Auth(m) | NetError::Bad(m) | NetError::No(m))
                if self.acc.oauth.as_deref() == Some("microsoft") && m.to_ascii_lowercase().contains("not connected") =>
            {
                m
            }
            Err(e) => return Err(e),
        };
        log::warn!("{}: {m}", self.acc.id);
        let other = self.acc.oauth_user.as_deref().filter(|o| !o.eq_ignore_ascii_case(user));
        if let Some(other) = other {
            let mut c = Client::connect(&self.acc.imap)?;
            if c.login_xoauth2(other, token).is_ok() {
                return Ok(c);
            }
        }
        Err(NetError::Refused(microsoft_imap_closed(&self.acc.email, other)))
    }

    /// A connection that couldn't be made is never tried again at once (that looped, showing
    /// "Connecting…" for good): a refusal (Graph's "no mailbox", a setting the server won't take)
    /// waits until asked, anything else for the backoff.
    fn connect_failed(&mut self, e: NetError) {
        match e {
            e @ NetError::Auth(_) => self.failed(e),
            e if e.is_fatal() => self.failed(e),
            e => {
                log::warn!("{}: {e}", self.acc.id);
                self.conn = None;
                self.auth_failed = true;
                self.status(SyncStatus::Error { error: e.to_string() });
            }
        }
    }

    /// Something every sync needs failed though the connection works (the folder list): let go
    /// and try again after a wait that grows each time, rather than at once and for good.
    fn retry_later(&mut self, what: &str) {
        let wait = self.relist_backoff;
        self.relist_backoff = (wait * 2).min(Duration::from_secs(300));
        if let Some(Conn::Imap(c)) = self.conn.as_mut() {
            c.logout();
        }
        self.conn = None;
        self.next_attempt = Instant::now() + wait;
        self.status(SyncStatus::Error { error: format!("{what} failed; trying again in {} s", wait.as_secs()) });
    }

    fn connect_and_sync(&mut self) {
        if let Err(e) = self.connect() {
            self.connect_failed(e);
            return;
        }
        self.replay();
        self.flush_outbox();
        // Drafts kept while offline go to the server first.
        let kept: Vec<PendingDraft> = self.store.load_account_doc(&self.acc.id, PENDING_DRAFTS).unwrap_or_default();
        if !kept.is_empty() {
            let _ = self.store.save_account_doc(&self.acc.id, PENDING_DRAFTS, &Vec::<PendingDraft>::new());
            for d in kept.into_iter().rev() {
                self.queue.push_front(Job::SaveDraft { account: self.acc.id.clone(), raw: d.raw, folder: d.folder, replaces: d.replaces });
            }
        }
        self.last_full = None;
    }

    fn client(&mut self) -> Result<&mut Client<Stream>> {
        match self.conn.as_mut() {
            Some(Conn::Imap(c)) => Ok(c),
            _ => Err(NetError::Closed("not connected".into())),
        }
    }

    /// Save the JMAP / Graph numbering after it changed.
    fn save_jstate(&self) {
        if self.acc.is_api()
            && let Err(e) = self.jstate.save(&self.store, &self.acc.id)
        {
            log::warn!("{}: {e}", self.acc.id);
        }
    }

    /// The folder list, then every folder (Inbox first), staying responsive to opened messages.
    /// Refresh the folder list from the server; `false` when that failed.
    fn relist(&mut self) -> bool {
        let id = self.id();
        let listed = match self.conn.as_mut() {
            Some(Conn::Imap(c)) => c.list(&id),
            Some(Conn::Jmap(c)) => jmap::list_folders(c, &mut self.jstate, &id),
            Some(Conn::Graph(c)) => crate::graph::list_folders(c, &mut self.jstate, &id),
            Some(Conn::Ews(c)) => crate::ews::list_folders(c, &mut self.jstate, &id),
            Some(Conn::Pop) => crate::pop::folders(&self.store, &id),
            None => Err(NetError::Closed("not connected".into())),
        };
        let jmap = matches!(self.conn, Some(Conn::Jmap(_) | Conn::Graph(_) | Conn::Ews(_)));
        if jmap {
            self.save_jstate();
        }
        match listed {
            Ok(mut list) => {
                // JMAP lists the counts; IMAP's come from each folder's sync.
                for f in list.iter_mut().filter(|_| !jmap) {
                    if let Some(old) = self.folders.iter().find(|o| o.path == f.path) {
                        f.unread = old.unread;
                        f.total = old.total;
                    }
                }
                sort_folders(&mut list);
                // Folders gone from the server lose their cache.
                for old in &self.folders {
                    if !list.iter().any(|f| f.path == old.path) {
                        let _ = self.store.clear_folder(&id, &old.path);
                    }
                }
                self.folders = list;
                self.publish_folders();
                true
            }
            Err(e) => {
                self.failed(e);
                false
            }
        }
    }

    fn sync_all(&mut self, rx: &Receiver<Job>) {
        let began = Instant::now();
        log::debug!("{}: reading every folder", self.acc.id);
        if !self.relist() {
            // Refused rather than cut off: without a folder list there's no sync, and asking
            // again at once looped (showing "Updating…" for good).
            if self.conn.is_some() {
                self.retry_later("reading the folder list");
            }
            return;
        }
        let paths: Vec<String> = self.folders.iter().filter(|f| f.selectable).map(|f| f.path.clone()).collect();
        for path in paths {
            self.drain(rx);
            // Opened messages and changes made here jump the queue: a change left until the
            // whole sync is done would be undone on screen by every folder read meanwhile.
            let urgent: Vec<Job> = self.queue.iter().filter(|j| matches!(j, Job::FetchBody { .. } | Job::Apply { .. })).cloned().collect();
            self.queue.retain(|j| !matches!(j, Job::FetchBody { .. } | Job::Apply { .. }));
            for j in urgent {
                self.handle(j);
            }
            if self.closed || self.stopping() || self.conn.is_none() {
                return;
            }
            self.sync_one(&path);
        }
        if self.conn.is_some() {
            self.report_quota();
        }
        if self.conn.is_some() {
            self.prune_jmap_numbers();
            self.last_full = Some(Instant::now());
            // Anything left from a failed attempt gets another try every full sync.
            self.flush_outbox();
            self.relist_backoff = Duration::from_secs(5);
            log::info!("{}: synced {} folders in {:.1} s", self.acc.id, self.folders.len(), began.elapsed().as_secs_f32());
            // The mail is up to date: said now. The calendars and contacts follow (on a big
            // Microsoft 365 calendar that takes minutes, and the account said "Updating…" all
            // along).
            self.status(SyncStatus::Online { at: now() });
        }
        if self.conn.is_some() {
            self.sync_dav();
        }
    }

    /// Forget the numbers of JMAP messages no longer cached (nor waiting in a change).
    fn prune_jmap_numbers(&mut self) {
        if !matches!(self.conn, Some(Conn::Jmap(_) | Conn::Graph(_) | Conn::Ews(_))) {
            return;
        }
        let mut keep: std::collections::HashSet<u32> =
            pending(&self.ops).iter().flat_map(|o| o.keys().iter().map(|k| k.uid).collect::<Vec<_>>()).collect();
        for f in &self.folders {
            if let Ok(st) = self.store.load_folder(&self.acc.id, &f.path) {
                keep.extend(st.messages.iter().map(|m| m.key.uid));
            }
        }
        self.jstate.prune(&keep);
        self.save_jstate();
    }

    fn publish_folders(&self) {
        let _ = self.store.save_folders(&self.acc.id, &self.folders);
        self.sink.emit(Event::Folders { account: self.id(), folders: self.folders.clone() });
    }

    fn sync_one(&mut self, path: &str) {
        let began = Instant::now();
        self.status(SyncStatus::Syncing { folder: path.to_string() });
        let (store, opts, id) = (self.store.clone(), self.opts, self.id());
        let result = match self.conn.as_mut() {
            Some(Conn::Imap(c)) => sync_folder(c, &store, &id, path, &opts),
            Some(Conn::Jmap(c)) => {
                let r = jmap::sync_folder(c, &mut self.jstate, &store, &id, path, &opts);
                self.save_jstate();
                r
            }
            Some(Conn::Graph(c)) => {
                let r = crate::graph::sync_folder(c, &mut self.jstate, &store, &id, path, &opts);
                self.save_jstate();
                r
            }
            Some(Conn::Ews(c)) => {
                let r = crate::ews::sync_folder(c, &mut self.jstate, &store, &id, path, &opts);
                self.save_jstate();
                r
            }
            // POP: the Inbox is downloaded; every other folder is here only.
            Some(Conn::Pop) if self.folders.iter().any(|f| f.path == path && f.role == FolderRole::Inbox) => {
                let secret = self.secret();
                (|| {
                    let mut c = crate::pop::open(&self.acc, &secret?)?;
                    let r = crate::pop::fetch_new(&mut c, &store, &id, path, &crate::pop::Keep::of(&self.acc), now())?;
                    let _ = c.quit();
                    Ok(r)
                })()
            }
            Some(Conn::Pop) => Ok(crate::pop::local_counts(&store, &id, path)),
            None => Err(NetError::Closed("not connected".into())),
        };
        // What the sync saved is the server's state: changes still on their way there go back on
        // top, before anything reads the folder (they were lost to the next start otherwise).
        let kept = if result.is_ok() { self.keep_pending(Some(path)) } else { Vec::new() };
        let took = began.elapsed();
        match &result {
            Ok(r) => {
                let line = format!(
                    "{}: {path}: read in {:.1} s ({} new, {} changed, {} removed{}; {} messages, {} unread){}",
                    self.acc.id,
                    took.as_secs_f32(),
                    r.new,
                    r.changed,
                    r.removed,
                    if r.reset { ", cache reset" } else { "" },
                    r.total,
                    r.unread,
                    if kept.is_empty() { "" } else { "; waiting changes kept" }
                );
                if r.touched() || took >= SLOW_SYNC {
                    log::info!("{line}");
                } else {
                    log::debug!("{line}");
                }
            }
            Err(_) => log::debug!("{}: {path}: failed after {:.1} s", self.acc.id, took.as_secs_f32()),
        }
        match result {
            Ok(r) => {
                let first = self.folders.iter().find(|f| f.path == path).is_some_and(|f| f.total == 0 && r.total > 0);
                if let Some(f) = self.folders.iter_mut().find(|f| f.path == path) {
                    let counts_changed = f.unread != r.unread || f.total != r.total;
                    f.unread = r.unread;
                    f.total = r.total;
                    if counts_changed {
                        self.publish_folders();
                    }
                }
                if r.touched() || first || !kept.is_empty() {
                    self.sink.emit(Event::FolderChanged { account: id, folder: path.to_string() });
                }
                if self.last_full.is_some() {
                    self.status(SyncStatus::Online { at: now() });
                }
            }
            Err(e) => {
                self.failed_in(path, e);
                // Refused, not cut off (the connection still works): said as a notice, and the
                // account is up to date otherwise. It stayed "Updating…" for good.
                if self.conn.is_some() && self.last_full.is_some() {
                    self.status(SyncStatus::Online { at: now() });
                }
            }
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::SyncAll { .. } => {
                if self.conn.is_none() {
                    self.auth_failed = false;
                    self.next_attempt = Instant::now();
                    self.connect_and_sync();
                }
                self.last_full = None;
            }
            Job::SyncFolder { folder, .. } => {
                if self.conn.is_none() {
                    self.next_attempt = Instant::now();
                    self.connect_and_sync();
                }
                if self.conn.is_some() {
                    self.sync_one(&folder);
                }
            }
            Job::FetchBody { key } => self.fetch(key),
            Job::Apply { op, .. } => {
                // POP: a move is carried out here (there's no server copy to come back); what's
                // deleted for good is noted, for removing the server's copy when the account says.
                if self.acc.pop.is_some() {
                    let gone: Vec<mailcraft_model::MessageKey> = match &op {
                        Op::Expunge { keys } => keys.clone(),
                        Op::EmptyFolder { path } => {
                            self.store.load_folder(&self.acc.id, path).map(|f| f.messages.into_iter().map(|m| m.key).collect()).unwrap_or_default()
                        }
                        _ => Vec::new(),
                    };
                    if let Err(e) = crate::pop::deleted_for_good(&self.store, &self.acc.id, &gone) {
                        self.notice(e.to_string());
                    }
                }
                let applied = match &op {
                    Op::Move { keys, to } if self.acc.pop.is_some() => crate::pop::move_locally(&self.store, &self.acc.id, keys, to),
                    _ => self.store.apply_op(&self.acc.id, &op).map_err(NetError::from),
                };
                match applied {
                    Ok(changed) => {
                        if op.is_folder_op() {
                            self.folders = self.store.load_folders(&self.acc.id).unwrap_or_default();
                            self.publish_folders();
                        }
                        for folder in changed {
                            self.sink.emit(Event::FolderChanged { account: self.id(), folder });
                        }
                    }
                    Err(e) => self.notice(e.to_string()),
                }
                // Kept in the pending changes by the pool already: sent from there.
                if self.conn.is_some() {
                    self.replay();
                }
            }
            Job::Send { id, raw, from, recipients, sent_folder, .. } => self.send(id, &raw, &from, &recipients, sent_folder),
            Job::SaveDraft { raw, folder, replaces, .. } => {
                let (raw_kept, replaces_kept) = (raw.clone(), replaces.clone());
                let r = (|| -> Result<()> {
                    if self.conn.is_none() {
                        self.connect()?;
                    }
                    if let Some(Conn::Pop) = self.conn.as_ref() {
                        let flags = mailcraft_model::Flags { draft: true, seen: true, ..Default::default() };
                        crate::pop::file_locally(&self.store, &self.acc.id, &folder, &raw, flags, now())?;
                        if let Some(old) = replaces {
                            self.store.apply_op(&self.acc.id, &Op::Expunge { keys: vec![old] })?;
                        }
                        return Ok(());
                    }
                    if let Some(Conn::Jmap(c)) = self.conn.as_mut() {
                        jmap::append(c, &self.jstate, &folder, &raw, &["$draft", "$seen"])?;
                        if let Some(old) = replaces {
                            jmap::apply_remote(c, &self.jstate, &Op::Expunge { keys: vec![old] })?;
                        }
                        return Ok(());
                    }
                    if let Some(Conn::Graph(c)) = self.conn.as_mut() {
                        crate::graph::append(c, &self.jstate, &folder, &raw)?;
                        if let Some(old) = replaces {
                            crate::graph::apply_remote(c, &self.jstate, &Op::Expunge { keys: vec![old] })?;
                        }
                        return Ok(());
                    }
                    if let Some(Conn::Ews(c)) = self.conn.as_mut() {
                        crate::ews::append(c, &self.jstate, &folder, &raw)?;
                        if let Some(old) = replaces {
                            crate::ews::apply_remote(c, &self.jstate, &Op::Expunge { keys: vec![old] })?;
                        }
                        return Ok(());
                    }
                    let trash = self.folders.iter().find(|f| f.role == FolderRole::Trash).map(|f| f.path.clone());
                    let c = self.client()?;
                    c.append(&folder, "\\Draft \\Seen", &raw)?;
                    if let Some(old) = replaces {
                        expunge_keys(c, &[old], trash.as_deref())?;
                    }
                    Ok(())
                })();
                match r {
                    Ok(()) => self.sync_one(&folder),
                    Err(e) => {
                        // Not lost: kept on this computer and saved when the account connects.
                        match keep_draft(&self.store, &self.acc.id, &folder, replaces_kept, raw_kept) {
                            Ok(()) => self.notice(
                                "the draft couldn't be saved to the server; it's kept on this computer and saved there when the account connects",
                            ),
                            Err(k) => self.notice(format!("the draft couldn't be saved: {k}")),
                        }
                        self.failed(e);
                    }
                }
            }
            Job::FlushOutbox { .. } => self.flush_outbox(),
            Job::SyncDav { .. } => self.sync_dav(),
            Job::GetVacation { .. } => {
                let r = self.vacation(None);
                self.report_vacation(r, None);
            }
            Job::SetVacation { vacation, .. } => {
                let r = self.vacation(Some(&vacation));
                match r {
                    Ok(v) => self.report_vacation(Ok(v), None),
                    // Show what the server still has, with why the change failed.
                    Err(e) => {
                        let back = self.vacation(None);
                        self.report_vacation(back, Some(e.to_string()));
                    }
                }
            }
            Job::FetchUrl { .. } | Job::Discover { .. } | Job::SignIn { .. } | Job::ImportPst { .. } | Job::ExportPst { .. } => {}
            Job::Start { .. } | Job::Remove { .. } | Job::WorkOffline { .. } | Job::SetSync { .. } => {}
        }
    }

    /// Read (and first change, when `set`) the automatic replies: JMAP's own, or a Sieve script
    /// through ManageSieve on the IMAP host.
    fn vacation(&mut self, set: Option<&mailcraft_model::job::Vacation>) -> Result<mailcraft_model::job::Vacation> {
        // Not connected: one try, and a failure shows as the account's status (it said
        // "Connecting…" for good when the vacation page was opened on an account that can't).
        // (The second connection asks ManageSieve straight away: it needs no IMAP session.)
        if self.conn.is_none() && (self.main.is_none() || self.acc.is_api()) {
            if self.auth_failed {
                return Err(NetError::Refused("it shows once the account is connected".into()));
            }
            if let Err(e) = self.connect() {
                if self.main.is_some() {
                    return Err(e);
                }
                self.connect_failed(e);
                return Err(NetError::Refused("it shows once the account is connected".into()));
            }
        }
        if let Some(Conn::Pop) = &self.conn {
            return Err(NetError::No("POP accounts have no automatic replies on the server".into()));
        }
        if let Some(Conn::Jmap(c)) = &self.conn {
            if let Some(v) = set {
                c.set_vacation(v)?;
            }
            return c.vacation();
        }
        if let Some(Conn::Graph(c)) = &self.conn {
            if let Some(v) = set {
                c.set_vacation(v)?;
            }
            return c.vacation();
        }
        if let Some(Conn::Ews(c)) = &self.conn {
            if let Some(v) = set {
                c.set_vacation(v)?;
            }
            return c.vacation();
        }
        let secret = self.secret()?;
        let user = if self.acc.username.is_empty() { self.acc.email.clone() } else { self.acc.username.clone() };
        let mut s = crate::sieve::Client::connect(&self.acc.imap, self.sieve_port).map_err(|e| match e {
            NetError::Io(_) => NetError::No("this server doesn't offer automatic replies (no ManageSieve)".into()),
            e => e,
        })?;
        if self.acc.oauth.is_some() {
            s.login_xoauth2(&user, &secret)?;
        } else {
            s.login(&user, &secret)?;
        }
        if let Some(v) = set {
            s.set_vacation(v)?;
        }
        let v = s.vacation();
        s.logout();
        v
    }

    fn report_vacation(&self, r: Result<mailcraft_model::job::Vacation>, failed: Option<String>) {
        let (vacation, error) = match r {
            Ok(v) => (Some(v), failed),
            Err(e) => (None, Some(failed.unwrap_or_else(|| e.to_string()))),
        };
        self.sink.emit(Event::Vacation { account: self.id(), vacation, error });
    }

    /// Sync calendars and address books with the account's CalDAV / CardDAV server (password
    /// accounts: OAuth tokens are for mail only).
    fn sync_dav(&mut self) {
        let began = Instant::now();
        // Exchange Online: its calendars and contacts through Graph.
        if let Some(Conn::Graph(c)) = &self.conn {
            let pim: mailcraft_model::pim::Pim = self.store.load_account_doc(&self.acc.id, mailcraft_model::pim::PIM_DOC).unwrap_or_default();
            let r = crate::graph_pim::sync(c, &pim, now());
            self.dav_done(r, began);
            return;
        }
        // On-premises Exchange: its calendar and contacts through EWS.
        if let Some(Conn::Ews(c)) = &self.conn {
            let pim: mailcraft_model::pim::Pim = self.store.load_account_doc(&self.acc.id, mailcraft_model::pim::PIM_DOC).unwrap_or_default();
            let r = crate::ews_pim::sync(c, &pim, now());
            self.dav_done(r, began);
            return;
        }
        // OAuth tokens are for mail only.
        if self.acc.oauth.is_some() || self.acc.ews.is_some() {
            return;
        }
        let pim: mailcraft_model::pim::Pim = self.store.load_account_doc(&self.acc.id, mailcraft_model::pim::PIM_DOC).unwrap_or_default();
        let user = if self.acc.username.is_empty() { self.acc.email.clone() } else { self.acc.username.clone() };
        let password = self.auth.lock().unwrap_or_else(std::sync::PoisonError::into_inner).secret.clone();
        let dav = crate::dav::Dav::new(&user, &password);
        let host = self.acc.pop.as_ref().map_or(self.acc.imap.host.as_str(), |p| p.host.as_str());
        let r = crate::dav::sync(&dav, &self.acc.email, host, &pim, now());
        if let Err(e @ NetError::Auth(_)) = &r {
            self.notice(format!("calendars and contacts: {e}"));
        }
        self.dav_done(r, began);
    }

    /// Calendars and contacts read: handed up, and said in the log with how long it took.
    fn dav_done(&mut self, r: Result<mailcraft_model::pim::DavSync>, began: Instant) {
        let took = began.elapsed().as_secs_f32();
        match r {
            Ok(sync) => {
                log::info!("{}: calendars and contacts read in {took:.1} s", self.acc.id);
                self.sink.emit(Event::DavSynced { account: self.id(), sync: Box::new(sync) });
            }
            Err(e) => log::warn!("{}: calendars and contacts failed after {took:.1} s: {e}", self.acc.id),
        }
    }

    /// Report the space used on the server (when it publishes a quota).
    fn report_quota(&mut self) {
        let q = match self.conn.as_mut() {
            Some(Conn::Imap(c)) => c.quota(),
            Some(Conn::Jmap(c)) => c.quota(),
            // Graph doesn't tell a mailbox's size to the mailbox's owner.
            Some(Conn::Graph(_) | Conn::Ews(_) | Conn::Pop) | None => return,
        };
        match q {
            Ok(Some((used, limit))) => self.sink.emit(Event::Quota { account: self.id(), used, limit }),
            Ok(None) => {}
            Err(e) if e.is_fatal() => self.failed(e),
            Err(e) => log::info!("{}: quota: {e}", self.acc.id),
        }
    }

    fn fetch(&mut self, key: MessageKey) {
        if let Some(raw) = self.store.read_body(&key) {
            self.sink.emit(Event::Body { key, raw });
            return;
        }
        let store = self.store.clone();
        let r = (|| -> Result<Vec<u8>> {
            if self.conn.is_none() {
                self.connect()?;
            }
            match self.conn.as_mut() {
                Some(Conn::Jmap(c)) => jmap::fetch_body(c, &self.jstate, &store, &key),
                Some(Conn::Graph(c)) => crate::graph::fetch_body(c, &self.jstate, &store, &key),
                Some(Conn::Ews(c)) => crate::ews::fetch_body(c, &self.jstate, &store, &key),
                // POP mail is downloaded whole: what isn't here isn't anywhere.
                Some(Conn::Pop) => store.read_body(&key).ok_or_else(|| NetError::No("this message isn't on this computer".into())),
                _ => fetch_body(self.client()?, &store, &key),
            }
        })();
        match r {
            Ok(raw) => {
                // The download saved the folder's list again: changes waiting stay in it.
                self.keep_pending(Some(&key.folder));
                self.sink.emit(Event::Body { key: key.clone(), raw });
                self.sink.emit(Event::FolderChanged { account: key.account.clone(), folder: key.folder.clone() });
            }
            Err(e) => {
                self.sink.emit(Event::BodyFailed { key, error: e.to_string() });
                if e.is_fatal() || matches!(e, NetError::Auth(_)) {
                    self.failed(e);
                }
            }
        }
    }

    /// Hand a message to the server: SMTP, or for JMAP accounts a submission (which also files
    /// the copy in Sent).
    fn deliver(&mut self, raw: &[u8], from: &str, recipients: &[String], sent_folder: Option<&str>) -> Result<()> {
        if self.acc.graph.is_some() {
            if self.conn.is_none() {
                self.connect()?;
            }
            let Some(Conn::Graph(c)) = &self.conn else { return Err(NetError::Closed("not connected".into())) };
            return crate::graph::send(c, raw, recipients);
        }
        if self.acc.ews.is_some() {
            if self.conn.is_none() {
                self.connect()?;
            }
            let Some(Conn::Ews(c)) = &self.conn else { return Err(NetError::Closed("not connected".into())) };
            return crate::ews::send(c, raw, recipients);
        }
        if self.acc.jmap.is_some() {
            if self.conn.is_none() {
                self.connect()?;
            }
            let sent = sent_folder.map(str::to_string).or_else(|| self.folders.iter().find(|f| f.role == FolderRole::Sent).map(|f| f.path.clone()));
            let Some(Conn::Jmap(c)) = &self.conn else { return Err(NetError::Closed("not connected".into())) };
            return jmap::send(c, &self.jstate, sent.as_deref(), raw, from, recipients);
        }
        let user = if self.acc.username.is_empty() { self.acc.email.clone() } else { self.acc.username.clone() };
        let secret = self.secret()?;
        smtp::send_with(&self.acc.smtp, &user, &secret, self.acc.oauth.is_some(), from, recipients, raw).map_err(|e| match e {
            // Say whether the sign-in covered sending: Microsoft lists what it granted.
            NetError::Auth(m) if self.acc.oauth.as_deref() == Some("microsoft") => {
                let granted = self.granted();
                let why = if granted.is_empty() {
                    String::new()
                } else if granted.to_ascii_lowercase().contains("smtp.send") {
                    " (the sign-in includes sending: Microsoft's server refused it)".to_string()
                } else {
                    format!(" (Microsoft's sign-in didn't include sending; it granted: {granted})")
                };
                NetError::Auth(format!("{m}{why}"))
            }
            e => e,
        })
    }

    fn send(&mut self, id: u64, raw: &[u8], from: &str, recipients: &[String], sent_folder: Option<String>) {
        let sent = self.deliver(raw, from, recipients, sent_folder.as_deref());
        if let Err(e) = sent {
            if e.is_retryable() {
                // Offline or asked to wait: keep it in the Outbox and send it on reconnect.
                let item = OutboxItem {
                    id,
                    from: from.to_string(),
                    recipients: recipients.to_vec(),
                    sent_folder,
                    queued: now(),
                    attempts: 1,
                    error: Some(e.to_string()),
                    not_before: 0,
                };
                match self.store.outbox_add(&self.acc.id, &item, raw) {
                    Ok(()) => self.sink.emit(Event::Queued { account: self.id(), id, error: e.to_string() }),
                    Err(se) => {
                        self.sink.emit(Event::SendFailed { account: self.id(), id, error: format!("{e} (and the Outbox couldn't keep it: {se})") })
                    }
                }
            } else {
                self.sink.emit(Event::SendFailed { account: self.id(), id, error: e.to_string() });
            }
            return;
        }
        self.after_sent(id, raw, sent_folder);
    }

    /// Send what waits in the Outbox, oldest first. Stops at the first failure that may clear
    /// up later (still offline); a message refused for good stays, marked with the error.
    fn flush_outbox(&mut self) {
        for mut item in self.store.outbox(&self.acc.id) {
            // Held back (Delay Delivery) until its time; then dated when it goes.
            if item.not_before > now() {
                continue;
            }
            let Some(raw) = self.store.outbox_raw(&self.acc.id, item.id) else { continue };
            let raw = if item.not_before > 0 { mailcraft_mime::with_date(&raw, now()) } else { raw };
            if item.error.as_deref().is_some_and(|e| e.starts_with("refused:")) {
                // Refused for good: waits for the user to edit or delete it.
                continue;
            }
            let sent = self.deliver(&raw, &item.from, &item.recipients, item.sent_folder.as_deref());
            match sent {
                Ok(()) => {
                    self.store.outbox_remove(&self.acc.id, item.id);
                    let folder = item.sent_folder.take();
                    self.after_sent(item.id, &raw, folder);
                }
                Err(e) => {
                    let retry = e.is_retryable();
                    item.attempts = item.attempts.saturating_add(1);
                    item.error = Some(if retry { e.to_string() } else { format!("refused: {e}") });
                    let _ = self.store.outbox_update(&self.acc.id, &item);
                    if retry {
                        self.sink.emit(Event::Queued { account: self.id(), id: item.id, error: e.to_string() });
                        return;
                    }
                    self.sink.emit(Event::SendFailed { account: self.id(), id: item.id, error: e.to_string() });
                }
            }
        }
    }

    /// A message went out: tell the engine and file a copy in Sent.
    fn after_sent(&mut self, id: u64, raw: &[u8], sent_folder: Option<String>) {
        self.sink.emit(Event::Sent { account: self.id(), id });
        if self.acc.is_api() {
            // Filed when it was submitted.
            let sent = sent_folder.or_else(|| self.folders.iter().find(|f| f.role == FolderRole::Sent).map(|f| f.path.clone()));
            if let Some(f) = sent.filter(|_| self.conn.is_some()) {
                self.sync_one(&f);
                // Graph and Exchange file the copy a moment later: look again then.
                if matches!(self.conn, Some(Conn::Graph(_) | Conn::Ews(_))) {
                    let now = Instant::now();
                    self.soon.retain(|(_, g)| *g != f);
                    self.soon.push((now + SENT_SOON, f.clone()));
                    self.soon.push((now + SENT_LATER, f));
                }
            }
            return;
        }
        // Gmail files sent mail itself; elsewhere keep a copy in Sent.
        let files_itself = self.acc.smtp.host.ends_with("gmail.com") || self.acc.smtp.host.ends_with("googlemail.com");
        if let Some(folder) = sent_folder.filter(|_| !files_itself) {
            let r = (|| -> Result<()> {
                if self.conn.is_none() {
                    self.connect()?;
                }
                // POP: Sent Items is on this computer.
                if let Some(Conn::Pop) = self.conn.as_ref() {
                    let flags = mailcraft_model::Flags { seen: true, ..Default::default() };
                    return crate::pop::file_locally(&self.store, &self.acc.id, &folder, raw, flags, now()).map(drop);
                }
                self.client()?.append(&folder, "\\Seen", raw).map(|_| ())
            })();
            match r {
                Ok(()) => self.sync_one(&folder),
                Err(e) => self.notice(format!("sent, but no copy was saved in Sent Items: {e}")),
            }
        }
    }

    /// A change made on the server (or refused there): no longer pending.
    fn done_with(&self, op: &Op) {
        let mut ops = pending(&self.ops);
        // Only this connection takes changes off the front; the pool adds them at the end.
        if ops.first() == Some(op) {
            ops.remove(0);
        }
        if let Err(e) = self.store.save_ops(&self.acc.id, &ops) {
            log::warn!("{}: the changes still to make couldn't be saved: {e}", self.acc.id);
        }
    }

    /// Replay offline changes on the server, oldest first.
    fn replay(&mut self) {
        let mut destinations: Vec<String> = Vec::new();
        let mut relist = false;
        let trash = self.folders.iter().find(|f| f.role == FolderRole::Trash).map(|f| f.path.clone());
        loop {
            let Some(op) = pending(&self.ops).first().cloned() else { break };
            let began = Instant::now();
            let r = match self.conn.as_mut() {
                Some(Conn::Imap(c)) => apply_remote_with(c, &op, trash.as_deref()),
                Some(Conn::Jmap(c)) => jmap::apply_remote(c, &self.jstate, &op),
                Some(Conn::Graph(c)) => crate::graph::apply_remote(c, &self.jstate, &op),
                Some(Conn::Ews(c)) => crate::ews::apply_remote(c, &self.jstate, &op),
                // POP: changes are made here only (the cache has them already).
                Some(Conn::Pop) => Ok(()),
                None => Err(NetError::Closed("not connected".into())),
            };
            match r {
                Ok(()) => {
                    log::info!("{}: made on the server in {} ms: {}", self.acc.id, began.elapsed().as_millis(), op.describe());
                    match &op {
                        Op::Move { to, .. } => destinations.push(to.clone()),
                        Op::RenameFolder { to, .. } => {
                            relist = true;
                            destinations.push(to.clone());
                        }
                        Op::CreateFolder { .. } | Op::DeleteFolder { .. } => relist = true,
                        Op::EmptyFolder { path } => destinations.push(path.clone()),
                        Op::SetFlag { .. } | Op::SetKeyword { .. } | Op::Expunge { .. } => {}
                    }
                    self.done_with(&op);
                }
                // Kept for the next connection.
                Err(e) if e.is_fatal() => {
                    log::info!("{}: kept for the next connection: {}", self.acc.id, op.describe());
                    return self.failed_in("making a change on the server", e);
                }
                Err(e) => {
                    // The server refused: drop the change and resync so the cache matches the
                    // server again (forcing a full flag check).
                    log::warn!("{}: a change was refused, so it's undone here: {}: {e}", self.acc.id, op.describe());
                    self.done_with(&op);
                    let mut folders: Vec<String> = op.keys().iter().map(|k| k.folder.clone()).collect();
                    folders.dedup();
                    for f in &folders {
                        if let Ok(mut st) = self.store.load_folder(&self.acc.id, f) {
                            st.highest_modseq = 0;
                            st.uid_next = 0;
                            let _ = self.store.save_folder(&self.acc.id, f, &st);
                        }
                    }
                    relist |= op.is_folder_op();
                    self.sink.emit(Event::OpFailed { account: self.id(), op, error: e.to_string() });
                    destinations.extend(folders);
                }
            }
        }
        if relist {
            self.relist();
        }
        destinations.sort();
        destinations.dedup();
        for d in destinations {
            if self.conn.is_some() {
                self.sync_one(&d);
            }
        }
    }
}

/// Group keys by folder, select each folder, and skip keys whose UIDVALIDITY is stale.
fn by_folder<S: std::io::Read + std::io::Write>(
    c: &mut Client<S>,
    keys: &[MessageKey],
    mut f: impl FnMut(&mut Client<S>, &str, &str) -> Result<()>,
) -> Result<()> {
    let mut folders: Vec<&str> = keys.iter().map(|k| k.folder.as_str()).collect();
    folders.sort_unstable();
    folders.dedup();
    for folder in folders {
        let info = c.select(folder, false)?;
        let uids: Vec<u32> = keys.iter().filter(|k| k.folder == folder && k.validity == info.validity).map(|k| k.uid).collect();
        if uids.is_empty() {
            continue;
        }
        for chunk in crate::imap::uid_chunks(&uids, 500) {
            f(c, folder, &uid_set(&chunk))?;
        }
    }
    Ok(())
}

/// Permanent delete. On Gmail (`X-GM-EXT-1`) an EXPUNGE in a label folder merely archives to
/// All Mail, so those messages are first moved into Trash (COPYUID gives their new UIDs) and
/// expunged in Trash.
fn expunge_keys<S: std::io::Read + std::io::Write>(c: &mut Client<S>, keys: &[MessageKey], trash: Option<&str>) -> Result<()> {
    let to_trash = trash.filter(|_| c.has("X-GM-EXT-1"));
    let mut in_trash: Vec<u32> = Vec::new();
    by_folder(c, keys, |c, folder, set| match to_trash {
        Some(t) if folder != t => {
            let uids = c.uid_move_to(set, t)?;
            if uids.is_empty() {
                log::info!("{folder}: moved to {t} for deleting, but the server didn't say where");
            }
            in_trash.extend(uids);
            Ok(())
        }
        _ => {
            c.uid_store(set, true, Flag::Deleted.imap())?;
            c.expunge(set)
        }
    })?;
    if let (Some(t), false) = (to_trash, in_trash.is_empty()) {
        c.select(t, false)?;
        for chunk in crate::imap::uid_chunks(&in_trash, 500) {
            let set = uid_set(&chunk);
            c.uid_store(&set, true, Flag::Deleted.imap())?;
            c.expunge(&set)?;
        }
    }
    Ok(())
}

/// Apply one change on the server.
pub fn apply_remote<S: std::io::Read + std::io::Write>(c: &mut Client<S>, op: &Op) -> Result<()> {
    apply_remote_with(c, op, None)
}

/// [`apply_remote`] with the account's Trash path, which permanent deletes on Gmail need.
pub fn apply_remote_with<S: std::io::Read + std::io::Write>(c: &mut Client<S>, op: &Op, trash: Option<&str>) -> Result<()> {
    match op {
        Op::SetFlag { keys, flag, on } => by_folder(c, keys, |c, _, set| c.uid_store(set, *on, flag.imap())),
        // A keyword that isn't an atom would make the command malformed: the engine never sends one.
        Op::SetKeyword { keys, keyword, on } if mailcraft_model::is_keyword(keyword) => {
            by_folder(c, keys, |c, _, set| c.uid_store(set, *on, keyword))
        }
        Op::SetKeyword { .. } => Ok(()),
        Op::Move { keys, to } => by_folder(c, keys, |c, folder, set| if folder == to { Ok(()) } else { c.uid_move(set, to) }),
        Op::Expunge { keys } => expunge_keys(c, keys, trash),
        Op::CreateFolder { path } => c.create(path),
        Op::RenameFolder { from, to } => c.rename(from, to),
        Op::DeleteFolder { path } => c.delete(path),
        Op::EmptyFolder { path } => c.empty(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sent Items asked for after each of several messages is read once.
    #[test]
    fn a_folder_waiting_to_be_read_isnt_queued_twice() {
        let mut q = VecDeque::new();
        let sent = Job::SyncFolder { account: "a".into(), folder: "Sent".into() };
        push_job(&mut q, sent.clone());
        push_job(&mut q, Job::SyncFolder { account: "a".into(), folder: "INBOX".into() });
        push_job(&mut q, sent.clone());
        push_job(&mut q, Job::GetVacation { account: "a".into() });
        push_job(&mut q, Job::GetVacation { account: "a".into() });
        assert_eq!(q.len(), 4, "only the folder read is merged");
    }

    #[test]
    fn memory_credentials() {
        let m = MemoryCredentials::default();
        let a = AccountConfig { id: "a1".into(), ..Default::default() };
        assert_eq!(m.get(&a), None);
        m.set(&a, "pw").unwrap();
        assert_eq!(m.get(&a).as_deref(), Some("pw"));
        m.delete(&a);
        assert_eq!(m.get(&a), None);
    }

    /// Microsoft can take a token and then refuse the mailbox ("User is authenticated but not
    /// connected"). The address the browser signed in as is tried; otherwise it's a refusal that
    /// says what to do, not a sign-in to ask for again.
    #[test]
    fn a_mailbox_microsoft_wont_open_says_why() {
        let st = crate::fake::State::new();
        st.lock().unwrap().not_connected.push("zoe@hotmail.example".into());
        let port = crate::fake::serve_tcp(&st);
        let store = crate::fake::tests_store("closed-mailbox");
        let pool = Pool::new(store.clone(), Arc::new(MemoryCredentials::default()));
        let mut acc = AccountConfig {
            id: "a1".into(),
            email: "zoe@hotmail.example".into(),
            imap: mailcraft_model::ServerConfig { host: "127.0.0.1".into(), port, security: mailcraft_model::Security::None },
            oauth: Some("microsoft".into()),
            ..Default::default()
        };
        let w = Worker::new(acc.clone(), Auth::shared("refresh"), store.clone(), pool.sink.clone(), SyncOptions::default());
        let e = w.imap_with_token("zoe@hotmail.example", crate::fake::GOOD_TOKEN).err().unwrap();
        assert!(e.to_string().starts_with(mailcraft_model::MICROSOFT_IMAP_CLOSED) && e.to_string().contains("IMAP"), "{e:?}");
        // The browser signed in as the mailbox's own address: that one opens it.
        acc.oauth_user = Some("me@studio.example".into());
        let w = Worker::new(acc.clone(), Auth::shared("refresh"), store.clone(), pool.sink.clone(), SyncOptions::default());
        assert!(w.imap_with_token("zoe@hotmail.example", crate::fake::GOOD_TOKEN).is_ok());
        // Signed in as another account that can't open it either: named in the refusal.
        acc.oauth_user = Some("someone@else.example".into());
        let w = Worker::new(acc, Auth::shared("refresh"), store, pool.sink.clone(), SyncOptions::default());
        let e = w.imap_with_token("zoe@hotmail.example", crate::fake::GOOD_TOKEN).err().unwrap();
        assert!(matches!(&e, NetError::Refused(m) if m.contains("signed in as someone@else.example")), "{e:?}");
        // A token that's simply refused is still a sign-in to ask for again.
        let w = Worker::new(
            AccountConfig { oauth_user: None, ..w.acc.clone() },
            Auth::shared("refresh"),
            crate::fake::tests_store("closed-mailbox-2"),
            pool.sink.clone(),
            SyncOptions::default(),
        );
        assert!(matches!(w.imap_with_token("zoe@hotmail.example", "expired"), Err(NetError::Auth(_))));
    }

    /// A MailCraft data file (`.mailcraft`) comes in through the same job as a `.pst`.
    #[test]
    fn a_mailcraft_data_file_is_imported() {
        let t = std::env::temp_dir().join(format!("mailcraft-worker-datafile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let old = Store::open_packed(t.join("old"), t.join("Old Files")).unwrap();
        old.save_accounts(&[AccountConfig { id: "a1".into(), email: "zoe@studio.example".into(), ..Default::default() }]).unwrap();
        old.save_folders("a1", &[Folder { path: "INBOX".into(), role: FolderRole::Inbox, ..Default::default() }]).unwrap();
        let k = MessageKey { account: "a1".into(), folder: "INBOX".into(), validity: 1, uid: 3 };
        let mut st = old.load_folder("a1", "INBOX").unwrap();
        st.messages.push(mailcraft_model::Summary { key: k, subject: "Kept".into(), ..Default::default() });
        old.save_folder("a1", "INBOX", &st).unwrap();
        let file = old.data_file("a1").unwrap();
        drop(old);
        let store = Store::open_packed(t.join("app"), t.join("Files")).unwrap();
        let mut p = Pool::new(store.clone(), Arc::new(MemoryCredentials::default()));
        p.submit(Job::ImportPst { account: "p1".into(), path: file.to_string_lossy().into_owned() });
        let end = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while Instant::now() < end && !seen.iter().any(|e| matches!(e, Event::Imported { done: true, .. })) {
            seen.extend(p.wait(Duration::from_millis(100)));
        }
        assert!(seen.iter().any(|e| matches!(e, Event::Imported { messages: 1, done: true, error: None, .. })), "{seen:?}");
        assert!(seen.iter().any(|e| matches!(e, Event::Folders { folders, .. } if folders.len() == 1)));
        assert_eq!(store.load_folder("p1", "INBOX").unwrap().messages[0].key.account, "p1");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// A sync reads the server's state, which doesn't have the changes still on their way there:
    /// they stay in the cache over it (RFC 4549). Flagged here while the connection was busy, the
    /// flag was gone after the next sync until the change reached the server, and for good if
    /// the app quit before then.
    #[test]
    fn a_sync_keeps_changes_still_waiting_for_the_server() {
        let mut d = crate::fake_graph::Data::default();
        let inbox = d.folder("Inbox", None, Some("inbox"));
        let id = d.deliver(
            &inbox,
            b"From: zoe@contoso.example\r\nSubject: Contract\r\nMessage-ID: <c1@x>\r\n\r\nHello.\r\n",
            true,
            "2026-10-01T09:00:00Z",
        );
        let server = crate::fake_graph::serve(d);
        let store = crate::fake::tests_store("worker-keeps-waiting");
        let pool = Pool::new(store.clone(), Arc::new(MemoryCredentials::default()));
        let acc = AccountConfig { id: "a1".into(), email: "me@contoso.example".into(), graph: Some(server.base.clone()), ..Default::default() };
        let mut w = Worker::new(acc, Auth::shared(crate::fake_graph::TOKEN), store.clone(), pool.sink.clone(), SyncOptions::default());
        w.connect().unwrap();
        assert!(w.relist());
        w.sync_one("INBOX");
        let flagged = || store.load_folder("a1", "INBOX").unwrap().messages.first().is_some_and(|m| m.flags.flagged);
        let key = store.load_folder("a1", "INBOX").unwrap().messages[0].key.clone();
        assert!(!flagged());

        // Flagged here; not made on the server yet (the connection hasn't got to it).
        pending(&w.ops).push(Op::SetFlag { keys: vec![key], flag: Flag::Flagged, on: true });
        w.sync_one("INBOX");
        assert!(flagged(), "kept over what the server said");
        assert!(server.data.lock().unwrap().message(&id).is_some_and(|m| !m.flagged), "the server doesn't have it yet");
        // Once the server has it, nothing's waiting and the server's state is the cache's.
        w.replay();
        assert!(pending(&w.ops).is_empty());
        assert!(server.data.lock().unwrap().message(&id).is_some_and(|m| m.flagged));
        w.sync_one("INBOX");
        assert!(flagged());
        let _ = std::fs::remove_dir_all(store.root());
    }

    /// A connection the pool let go (Work offline, a new connection for the account) says nothing
    /// of the failures that causes: its "Error" came after the pool's "Offline", and stayed.
    #[test]
    fn a_connection_let_go_says_nothing() {
        let store = crate::fake::tests_store("let-go-quiet");
        let pool = Pool::new(store.clone(), Arc::new(MemoryCredentials::default()));
        let acc = AccountConfig { id: "a1".into(), ..Default::default() };
        let mut w = Worker::new(acc, Auth::shared("pw"), store.clone(), pool.sink.clone(), SyncOptions::default());
        w.stop.store(true, Ordering::Relaxed);
        w.failed_in("INBOX", NetError::Closed("the connection was let go".into()));
        w.status(SyncStatus::Connecting);
        assert!(pool.poll().is_empty());
        assert!(w.conn.is_none() && !w.auth_failed);
        let _ = std::fs::remove_dir_all(store.root());
    }

    #[test]
    fn pool_without_password_asks_for_one() {
        let dir = std::env::temp_dir().join(format!("mailcraft-pool-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let mut p = Pool::new(store, Arc::new(MemoryCredentials::default()));
        let a = AccountConfig { id: "a1".into(), ..Default::default() };
        p.submit(Job::Start { account: a, password: None });
        assert!(matches!(p.poll().as_slice(), [Event::Status { status: SyncStatus::NeedsPassword { .. }, .. }]));
        p.submit(Job::SyncAll { account: "a1".into() });
        assert!(matches!(p.poll().as_slice(), [Event::Notice { .. }]));
        let key = MessageKey { account: "a1".into(), folder: "INBOX".into(), validity: 1, uid: 1 };
        p.submit(Job::FetchBody { key: key.clone() });
        assert!(matches!(p.poll().as_slice(), [Event::BodyFailed { .. }]));
        p.store.write_body(&key, b"Subject: cached\r\n\r\nx").unwrap();
        p.submit(Job::FetchBody { key });
        assert!(matches!(p.poll().as_slice(), [Event::Body { .. }]));
        let _ = std::fs::remove_dir_all(dir);
    }
}
