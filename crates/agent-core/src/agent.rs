//! The agent the shell drives: pairing, the poll loop, and running what a call names.
//!
//! One [`Agent`] per process. The shell spawns [`Agent::run`] and calls the rest from its
//! window and menu: [`Agent::status`], [`Agent::start_pairing`], [`Agent::cancel_pairing`],
//! [`Agent::allow`], [`Agent::revoke`], [`Agent::allow_skill`], [`Agent::revoke_skill`],
//! [`Agent::skill_folder`], [`Agent::add_folder`], [`Agent::update_folder`],
//! [`Agent::remove_folder`], [`Agent::set_paused`], [`Agent::unpair`], [`Agent::set_cloud_url`].
//!
//! What binds (the agent-link protocol):
//!
//! - **A call carries a program's name and its blanks' values, nothing else.** What runs
//!   is the approved copy's resolved path with the approved copy's arguments
//!   ([`crate::approvals`]); a program the person has not allowed here as it is now is
//!   refused, and nothing runs.
//! - **A `skill_run` call carries a skill's name, a script's name and arguments, nothing
//!   else.** What runs is the script `SKILL.md` declared, from the approved copy of the
//!   skill's folder, as an argument list: the declared command's words, then the arguments.
//! - **A `folder_call` carries a folder tool's name, a folder's id and the model's
//!   arguments.** It acts only in a folder the person picked here, with the permission they
//!   gave it ([`crate::folders`]); an id this app does not have is refused.
//! - Four ops: `ping`, `local_run`, `skill_run` and `folder_call`. Anything else is refused.
//! - The secret is sent only to the cloud the link was made with.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::AbortHandle;

use crate::approvals::{AgentName, ApprovedScript, ApprovedSkill, Link, Standing, State, Store};
use crate::cloud::{self, Source};
use crate::folders::{self, Folder, Mode};
use crate::link::{self, Call, Client, Failure, Poll};
use crate::programs::{self, Program};
use crate::secrets::SecretStore;
use crate::skills::{self, Entry, SyncedFile};

/// The times the link keeps. The app uses [`Timing::default`]; the tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The long poll's `wait`, in seconds.
    pub wait_secs: u64,
    /// How long to wait after the first, second, third and every later failed poll.
    pub backoff: [Duration; 4],
    /// How long a program may run.
    pub time_limit: Duration,
    /// The least time between two polls that brought no call (a cloud that answers at
    /// once must not be asked in a spin).
    pub min_gap: Duration,
    /// The limit of a short request: starting a pairing, a call's answer, unpairing.
    pub quick: Duration,
    /// What a poll may take beyond its `wait`.
    pub grace: Duration,
    /// The limit of downloading one of a skill's files.
    pub download: Duration,
    /// How long the screen is let settle after an action before its new screenshot.
    pub settle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            wait_secs: 20,
            backoff: [Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)],
            time_limit: programs::TIME_LIMIT,
            min_gap: Duration::from_secs(1),
            quick: Duration::from_secs(10),
            grace: Duration::from_secs(15),
            download: Duration::from_secs(60),
            settle: Duration::from_millis(600),
        }
    }
}

impl Timing {
    /// The wait after `failures` failed polls in a row (0: the first failure).
    pub fn backoff_after(&self, failures: usize) -> Duration {
        self.backoff[failures.min(self.backoff.len() - 1)]
    }
}

/// Everything the agent is handed. **Nothing here falls back to a real default**: the
/// shell names the real folders, Credential Manager and environment; a test names temporary ones.
pub struct Options {
    /// The folder of the state file.
    pub state_dir: PathBuf,
    /// The person's home folder, where programs run.
    pub home: PathBuf,
    /// The `PATH` programs are looked for on when one is allowed
    /// ([`programs::search_path`] of the app's own).
    pub path: Option<OsString>,
    /// `WYSEBEE_CLOUD_URL`, when set: the default, which an address saved from the
    /// window wins over.
    pub cloud_url: Option<String>,
    /// Where the link's secret is kept.
    pub secrets: Arc<dyn SecretStore>,
    /// This PC's name.
    pub device_name: String,
    /// The app's version, as sent to the cloud.
    pub version: String,
    pub timing: Timing,
    /// The display, the mouse and the keyboard (screen.rs); none: this PC has no screen to show.
    pub screen: Option<Arc<dyn crate::screen::Screen>>,
}

/// One program in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgramStatus {
    pub name: String,
    pub description: String,
    /// The whole command line, blanks as `<name>`.
    pub command_line: String,
    pub status: Standing,
    /// The program's absolute path: the approved one, else where it is found now (none:
    /// not on this PC).
    pub path: Option<String>,
}

/// A script of a skill, as the window shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScriptStatus {
    pub name: String,
    pub command: String,
    pub description: String,
}

/// A synced file that differs from what was allowed (or put in the folder) last.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileChange {
    pub path: String,
    /// `added`, `changed` or `removed`.
    pub change: &'static str,
}

/// One skill in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkillStatus {
    pub name: String,
    pub status: Standing,
    /// Its declared scripts: the approved copy's, else as its downloaded `SKILL.md` declares
    /// them (none: not read yet, or it cannot be read — `note` says why).
    pub scripts: Option<Vec<ScriptStatus>>,
    pub note: Option<String>,
    /// What changed since it was allowed, or since its files were last put here (all
    /// `added` for a skill never put here).
    pub changes: Vec<FileChange>,
    /// Its folder on this PC (made by "Show in File Explorer" when it is not there yet).
    pub folder: String,
    /// Every file of it, as the cloud lists it now, is on this PC: in its approved copy, or
    /// downloaded, checked and kept for Allow (which then needs no download).
    pub stored: bool,
    /// Why its files could not all be downloaded, the last time it was tried.
    pub download_error: Option<String>,
}

/// One folder in the window, as the poll reports it, with its number (what Wysebee Desktop
/// numbers it as for the model) and whether it is there now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FolderStatus {
    pub id: String,
    pub number: usize,
    pub name: String,
    pub path: String,
    pub mode: Mode,
    pub description: String,
    pub present: bool,
}

/// What the window shows. `state` is `unpaired`, `waiting` (for the code to be typed),
/// `refused` or `paired`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    pub state: &'static str,
    /// Why the app is unpaired again, when it has something to say.
    pub note: Option<String>,
    /// `waiting`: the code as shown, `XXXX-XXXX`.
    pub code: Option<String>,
    /// `waiting`: seconds until the code runs out.
    pub expires_in: Option<u64>,
    /// The agent name the person typed.
    pub name: Option<String>,
    /// `refused`: why, in the cloud's words.
    pub message: Option<String>,
    /// `paired`: the agent.
    pub agent: Option<AgentName>,
    /// Whether the last poll reached Wysebee Cloud.
    pub connected: bool,
    pub paused: bool,
    /// "Let Wysebee run any program on this PC".
    pub any_program: bool,
    /// "Let Wysebee see and control this PC's screen".
    pub screen_control: bool,
    /// "Let Wysebee use this PC as a developer".
    pub dev_access: bool,
    /// "Let Wysebee use Android phones on this PC".
    pub android_dev: bool,
    pub programs: Vec<ProgramStatus>,
    pub skills: Vec<SkillStatus>,
    pub folders: Vec<FolderStatus>,
    /// How many programs and skills wait for the person's Allow.
    pub waiting: usize,
    /// The skills' files are being downloaded now.
    pub storing: bool,
    pub cloud_url: String,
    pub cloud_url_source: Source,
    /// What "Use the default" goes back to: `WYSEBEE_CLOUD_URL`, else the built-in one.
    pub cloud_default: String,
    pub device_name: String,
    pub version: String,
}

/// What one turn of the loop did.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// No link: nothing to ask.
    Idle,
    /// Waiting for the code to be typed.
    Pending,
    /// Paired; `calls` were handed out and started.
    Paired { calls: usize },
    /// The claim was refused; the pairing is forgotten.
    Refused,
    /// 401, or the secret is gone: the pairing is forgotten.
    Unpaired,
    /// 410: the code ran out; the pairing is forgotten.
    Expired,
    /// The poll failed; the loop backs off.
    Failed(String),
}

pub const PAUSED: &str = "Wysebee Agent is paused on that PC. The person can turn 'Let Wysebee run programs on this PC' back on in Wysebee Agent.";
/// A `program_run` while "Let Wysebee run any program on this PC" is off.
/// A screen call while "Let Wysebee see and control this PC's screen" is off.
pub const SCREEN_OFF: &str = "Screen control is off on that PC. The person can turn on 'Let Wysebee see and control this PC's screen' in Wysebee Agent.";
/// A developer call while "Let Wysebee use this PC as a developer" is off.
pub const DEV_OFF: &str = "Developer access is off on that PC. The person can turn on 'Let Wysebee use this PC as a developer' in Wysebee Agent.";
/// An Android call while "Let Wysebee use Android phones on this PC" is off.
pub const ANDROID_OFF: &str = "The Android toolkit is off on that PC. The person can turn on 'Let Wysebee use Android phones on this PC' in Wysebee Agent.";
pub const ANY_OFF: &str = "Running any program is off on that PC. Only the programs the person allowed run there; they can turn on 'Let Wysebee run any program on this PC' in Wysebee Agent.";

struct Runtime {
    secret: Option<String>,
    connected: bool,
    refused: Option<String>,
    note: Option<String>,
    /// The calls running now, by id.
    running: HashMap<String, AbortHandle>,
    /// The programs seen waiting, so the person is asked to look once per program.
    seen_waiting: HashSet<String>,
    /// Skills' files are being downloaded beside the loop ([`Agent::store_skills`]); `again`:
    /// the cloud's list changed meanwhile, so it looks once more when done.
    storing: bool,
    again: bool,
    /// Why a skill's files could not all be downloaded, by skill.
    download_errors: HashMap<String, String>,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    opts: Options,
    store: Store,
    client: Client,
    rt: Mutex<Runtime>,
    /// Cuts the poll short (the status changed) and wakes an idle loop.
    kick: Notify,
    /// One pairing starts at a time.
    pairing: tokio::sync::Mutex<()>,
    /// One skill is allowed at a time.
    skilling: tokio::sync::Mutex<()>,
    attention: Mutex<Option<Hook>>,
    /// Developer access's background jobs.
    jobs: crate::dev::Jobs,
}

#[derive(Clone)]
pub struct Agent(Arc<Inner>);

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// What makes a waiting program the one it is: what approval looks at.
fn waiting_key(p: &Program) -> String {
    json!([p.name, p.command, p.parts()]).to_string()
}

fn waiting_keys(state: &State) -> HashSet<String> {
    let programs = state.programs().into_iter().filter(|p| matches!(state.standing(p), Standing::Waiting | Standing::Changed)).map(|p| waiting_key(&p));
    let skills = state.skills().into_iter().filter(|e| matches!(state.skill_standing(e), Standing::Waiting | Standing::Changed)).map(|e| json!(["skill", e.name, skills::identity(&e.files)]).to_string());
    programs.chain(skills).collect()
}

fn needs_allow(s: Standing) -> bool {
    matches!(s, Standing::Waiting | Standing::Changed)
}

/// How `now` differs from `before`, by path.
fn changes(before: &[SyncedFile], now: &[SyncedFile]) -> Vec<FileChange> {
    let mut out = vec![];
    for f in now {
        match before.iter().find(|b| b.path == f.path) {
            None => out.push(FileChange { path: f.path.clone(), change: "added" }),
            Some(b) if b.id != f.id => out.push(FileChange { path: f.path.clone(), change: "changed" }),
            Some(_) => {}
        }
    }
    for b in before.iter().filter(|b| !now.iter().any(|f| f.path == b.path)) {
        out.push(FileChange { path: b.path.clone(), change: "removed" });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn script_status(s: &skills::Script) -> ScriptStatus {
    ScriptStatus { name: s.name.clone(), command: s.command.clone(), description: s.description.clone() }
}

impl Agent {
    pub fn open(opts: Options) -> Agent {
        let store = Store::open(&opts.state_dir);
        let seen_waiting = waiting_keys(&store.get());
        Agent(Arc::new(Inner {
            opts,
            store,
            client: Client::new(),
            rt: Mutex::new(Runtime { secret: None, connected: false, refused: None, note: None, running: HashMap::new(), seen_waiting, storing: false, again: false, download_errors: HashMap::new() }),
            kick: Notify::new(),
            pairing: tokio::sync::Mutex::new(()),
            skilling: tokio::sync::Mutex::new(()),
            attention: Mutex::new(None),
            jobs: crate::dev::Jobs::default(),
        }))
    }

    /// Called when a program starts waiting for the person's Allow (the shell shows the
    /// window).
    pub fn set_attention_hook(&self, hook: Option<Hook>) {
        *self.0.attention.lock().unwrap() = hook;
    }

    /// The state as kept in the file.
    pub fn state(&self) -> State {
        self.0.store.get()
    }

    /// The cloud a new pairing is made with, and where that address came from.
    pub fn cloud(&self) -> (String, Source) {
        let saved = self.0.store.get().cloud_url;
        cloud::choose(self.0.opts.cloud_url.as_deref(), saved.as_deref())
    }

    pub fn status(&self) -> Status {
        let state = self.0.store.get();
        let rt = self.0.rt.lock().unwrap();
        let path = self.0.opts.path.as_deref();
        let programs: Vec<ProgramStatus> = state
            .programs()
            .iter()
            .map(|p| {
                let status = state.standing(p);
                let at = match (status, state.approval(&p.name)) {
                    (Standing::Approved | Standing::MissingProgram, Some(a)) => Some(a.path.clone()),
                    _ => programs::resolve(&p.command, path),
                };
                ProgramStatus { name: p.name.clone(), description: p.description.clone(), command_line: p.command_line(), status, path: at.map(|p| p.display().to_string()) }
            })
            .collect();
        let skills: Vec<SkillStatus> = state.skills().iter().map(|e| self.skill_status(&state, e, rt.download_errors.get(&e.name))).collect();
        let (cloud_url, cloud_url_source) = match &state.link {
            // A link keeps the cloud it was made with.
            Some(l) => (l.cloud_url.clone(), cloud::choose(self.0.opts.cloud_url.as_deref(), state.cloud_url.as_deref()).1),
            None => cloud::choose(self.0.opts.cloud_url.as_deref(), state.cloud_url.as_deref()),
        };
        let link = state.link.as_ref();
        let paired = link.is_some_and(|l| l.agent.is_some());
        Status {
            state: match (link, &rt.refused) {
                (Some(_), _) if paired => "paired",
                (Some(_), _) => "waiting",
                (None, Some(_)) => "refused",
                (None, None) => "unpaired",
            },
            note: if link.is_none() { rt.note.clone() } else { None },
            code: link.filter(|_| !paired).and_then(|l| l.code.as_deref().map(link::shown_code)),
            expires_in: link.filter(|_| !paired).and_then(|l| l.expires_at).map(|at| at.saturating_sub(now())),
            name: link.map(|l| l.name.clone()),
            message: if link.is_none() { rt.refused.clone() } else { None },
            agent: link.and_then(|l| l.agent.clone()),
            connected: link.is_some() && rt.connected,
            paused: state.paused,
            any_program: state.any_program,
            screen_control: state.screen_control,
            dev_access: state.dev_access,
            android_dev: state.android_dev,
            waiting: programs.iter().filter(|p| needs_allow(p.status)).count() + skills.iter().filter(|s| needs_allow(s.status)).count(),
            storing: rt.storing,
            programs,
            skills,
            folders: state.folders.iter().enumerate().map(|(i, f)| FolderStatus { id: f.id.clone(), number: i + 1, name: f.name.clone(), path: f.path.display().to_string(), mode: f.mode, description: f.description.clone(), present: f.path.is_dir() }).collect(),
            cloud_url,
            cloud_url_source,
            cloud_default: cloud::default_url(self.0.opts.cloud_url.as_deref()).0,
            device_name: link::device_name(&self.0.opts.device_name),
            version: self.0.opts.version.clone(),
        }
    }

    /// The folder of the approved copies of skills.
    fn skills_dir(&self) -> PathBuf {
        self.0.opts.state_dir.join(skills::FOLDER)
    }

    /// Downloaded, checked files by id: staging, never run.
    fn cache_dir(&self) -> PathBuf {
        self.0.opts.state_dir.join(skills::CACHE)
    }

    fn skill_status(&self, state: &State, e: &Entry, download_error: Option<&String>) -> SkillStatus {
        let status = state.skill_standing(e);
        let approval = state.skill_approval(&e.name);
        let (scripts, note) = match (status, approval) {
            (Standing::Approved | Standing::MissingProgram, Some(a)) => (Some(a.scripts.iter().map(|s| ScriptStatus { name: s.name.clone(), command: s.command.clone(), description: s.description.clone() }).collect()), None),
            _ => match e.file(skills::SKILL_FILE) {
                None => (None, Some("It has no SKILL.md.".to_string())),
                Some(f) => match std::fs::read(self.cache_dir().join(&f.id)) {
                    Err(_) => (None, None),
                    Ok(bytes) => match skills::parse(&String::from_utf8_lossy(&bytes), &e.name) {
                        Ok(m) => (Some(m.scripts.iter().map(script_status).collect()), None),
                        Err(why) => (None, Some(why)),
                    },
                },
            },
        };
        let before = match approval {
            Some(a) => a.files.as_slice(),
            None => state.placed(&e.name),
        };
        let stored = !needs_allow(status) || e.files.iter().all(|f| self.cache_dir().join(&f.id).is_file());
        SkillStatus {
            name: e.name.clone(),
            status,
            scripts,
            note,
            changes: changes(before, &e.files),
            folder: self.skills_dir().join(&e.name).display().to_string(),
            stored,
            download_error: download_error.filter(|_| !stored).cloned(),
        }
    }

    /// The link's secret: remembered, else read from the store.
    fn secret(&self) -> Result<Option<String>, String> {
        if let Some(s) = self.0.rt.lock().unwrap().secret.clone() {
            return Ok(Some(s));
        }
        let read = self.0.opts.secrets.get()?;
        self.0.rt.lock().unwrap().secret = read.clone();
        Ok(read)
    }

    /// The pairing is over here: the link, the programs, their approvals and the secret
    /// go, and whatever runs is stopped.
    fn forget(&self, note: Option<&str>) {
        self.0.store.update(|s| s.forget_link());
        self.0.jobs.stop_all();
        // The skills' copies go with the pairing, the person's own files in them too.
        for dir in [self.skills_dir(), self.cache_dir()] {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, "skills_not_removed");
                }
            }
        }
        if let Err(e) = self.0.opts.secrets.delete() {
            tracing::warn!(error = %e, "secret_not_deleted");
        }
        let mut rt = self.0.rt.lock().unwrap();
        rt.secret = None;
        rt.connected = false;
        rt.note = note.map(str::to_string);
        rt.seen_waiting.clear();
        rt.download_errors.clear();
        for (_, running) in rt.running.drain() {
            running.abort();
        }
    }

    /// Start pairing as a new agent called `typed`: make the secret, ask the cloud for a
    /// code. The code is then in [`Agent::status`].
    pub async fn start_pairing(&self, typed: &str) -> Result<(), String> {
        let name = link::agent_name(typed)?;
        let _one = self.0.pairing.lock().await;
        if self.0.store.get().link.is_some() {
            return Err("This PC is already paired, or waiting for its code.".into());
        }
        let (base, _) = self.cloud();
        let secret = link::new_secret();
        let t = &self.0.opts.timing;
        let started = self.0.client.start(&base, &name, &secret, &self.0.opts.device_name, &self.0.opts.version, t.quick).await.map_err(|f| f.message().to_string())?;
        self.0.opts.secrets.set(&secret)?;
        self.0.store.update(|s| {
            s.forget_link();
            s.link = Some(Link { id: started.id.clone(), cloud_url: base.clone(), name, code: Some(started.code.clone()), expires_at: Some(now() + started.expires_in), agent: None });
        });
        {
            let mut rt = self.0.rt.lock().unwrap();
            rt.secret = Some(secret);
            rt.refused = None;
            rt.note = None;
            rt.connected = true;
        }
        tracing::info!(link = %started.id, "pairing_started");
        self.0.kick.notify_one();
        Ok(())
    }

    /// Tell the cloud the link is over (best effort), whether it was reached.
    async fn tell_unpaired(&self, link: &Link) -> bool {
        let Ok(Some(secret)) = self.secret() else { return false };
        self.0.client.unpair(&link.cloud_url, &link.id, &secret, self.0.opts.timing.quick).await.is_ok()
    }

    /// Stop waiting for the code to be typed, or dismiss a refusal. A paired PC is left
    /// as it is ([`Agent::unpair`]).
    pub async fn cancel_pairing(&self) {
        {
            let mut rt = self.0.rt.lock().unwrap();
            rt.refused = None;
            rt.note = None;
        }
        let _one = self.0.pairing.lock().await;
        if let Some(link) = self.0.store.get().link.filter(|l| l.agent.is_none()) {
            self.tell_unpaired(&link).await;
            self.forget(None);
            self.0.kick.notify_one();
        }
    }

    /// Unpair this PC. The agent stays in Wysebee, without it.
    pub async fn unpair(&self) {
        let _one = self.0.pairing.lock().await;
        if let Some(link) = self.0.store.get().link {
            let told = self.tell_unpaired(&link).await;
            self.forget((!told).then_some("Wysebee Cloud could not be told. Unpair this PC in Wysebee too, under Devices."));
            tracing::info!(told, "unpaired_here");
            self.0.kick.notify_one();
        }
    }

    /// The person allowed `name` as it is shown now.
    pub fn allow(&self, name: &str) -> Result<(), String> {
        let path = self.0.opts.path.as_deref();
        self.0.store.update(|s| s.allow(name, path))?;
        tracing::info!(program = name, "program_allowed");
        self.0.kick.notify_one();
        Ok(())
    }

    /// The person took `name`'s approval back.
    pub fn revoke(&self, name: &str) {
        self.0.store.update(|s| s.revoke(name));
        // It waits again, and the person knows: no need to ask them to look.
        self.0.rt.lock().unwrap().seen_waiting = waiting_keys(&self.0.store.get());
        self.0.kick.notify_one();
    }

    /// A skill's file in the cache, downloaded and checked when it is not there yet.
    async fn fetch(&self, link: &Link, secret: &str, file: &SyncedFile) -> Result<(), String> {
        let at = self.cache_dir().join(&file.id);
        if std::fs::read(&at).is_ok_and(|b| skills::hash_ok(&b, &file.id)) {
            return Ok(());
        }
        let t = &self.0.opts.timing;
        let bytes = self.0.client.skill_file(&link.cloud_url, &link.id, secret, &file.id, skills::MAX_FILE_BYTES, t.download).await.map_err(|f| format!("'{}' could not be downloaded: {}", file.path, f.message()))?;
        if !skills::hash_ok(&bytes, &file.id) {
            return Err(format!("'{}' did not arrive as Wysebee Cloud listed it: its contents do not match. Nothing was allowed.", file.path));
        }
        let io = |e: std::io::Error| format!("'{}' could not be kept: {e}", file.path);
        std::fs::create_dir_all(self.cache_dir()).map_err(io)?;
        let part = at.with_extension("part");
        std::fs::write(&part, &bytes).map_err(io)?;
        std::fs::rename(&part, &at).map_err(io)
    }

    /// Keep every listed skill's files on this PC: **as soon as
    /// the person changes a skill in Wysebee, this PC downloads it** — beside the loop, so a
    /// large skill never holds up the poll (the cloud would think the PC offline) — and
    /// keeps it in the cache, checked against its id. **Nothing of it runs until the person
    /// allows it** here, as before; Allow then needs no download. One run at a time: a list
    /// that changes meanwhile is looked at again when the run ends.
    fn store_skills(&self, link: Link, secret: String) {
        {
            let mut rt = self.0.rt.lock().unwrap();
            if rt.storing {
                rt.again = true;
                return;
            }
            rt.storing = true;
        }
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.store_once(&link, &secret).await;
                let mut rt = me.0.rt.lock().unwrap();
                if !std::mem::take(&mut rt.again) {
                    rt.storing = false;
                    return;
                }
            }
        });
    }

    /// One look at the cloud's list: each file of every skill that waits for Allow — waiting
    /// or changed — downloaded unless it is here already (the cache holds only checked files),
    /// `SKILL.md` first so the window can show its scripts; then the files no listed skill has
    /// dropped from the cache. An allowed skill's files are in its folder already. Nothing here
    /// runs, and files in the cache are not executable.
    async fn store_once(&self, link: &Link, secret: &str) {
        let state = self.0.store.get();
        let entries = state.skills();
        for e in entries.iter().filter(|e| needs_allow(state.skill_standing(e))) {
            let mut files: Vec<&SyncedFile> = e.files.iter().collect();
            files.sort_by_key(|f| f.path != skills::SKILL_FILE);
            let mut failed = None;
            for f in files {
                if self.cache_dir().join(&f.id).is_file() {
                    continue;
                }
                let fetched = self.fetch(link, secret, f).await;
                // Unpaired while it came: nothing of it is kept (forget removed the cache).
                if self.0.store.get().link.map(|l| l.id) != Some(link.id.clone()) {
                    let _ = std::fs::remove_dir_all(self.cache_dir());
                    return;
                }
                if let Err(error) = fetched {
                    tracing::warn!(skill = %e.name, %error, "skill_not_stored");
                    failed = Some(error);
                    break;
                }
            }
            let mut rt = self.0.rt.lock().unwrap();
            match failed {
                Some(error) => rt.download_errors.insert(e.name.clone(), error),
                None => rt.download_errors.remove(&e.name),
            };
        }
        let keep: HashSet<&str> = entries.iter().flat_map(|e| e.files.iter().map(|f| f.id.as_str())).collect();
        for entry in std::fs::read_dir(self.cache_dir()).into_iter().flatten().flatten() {
            if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// The person allowed the skill `name` as the cloud lists it now: its files downloaded
    /// and checked, its `SKILL.md` read, put in place in its folder (the person's own files
    /// there kept), and each declared script's program resolved — that is what runs.
    pub async fn allow_skill(&self, name: &str) -> Result<(), String> {
        let _one = self.0.skilling.lock().await;
        let state = self.0.store.get();
        let entry = state.skill(name).ok_or(format!("There is no skill '{name}' to allow."))?;
        let link = state.link.clone().filter(|l| l.agent.is_some()).ok_or("This PC is not paired.")?;
        let secret = self.secret()?.ok_or("This PC is not paired.")?;
        for f in &entry.files {
            self.fetch(&link, &secret, f).await?;
        }
        let md = entry.file(skills::SKILL_FILE).ok_or(format!("The skill '{name}' has no SKILL.md."))?;
        let text = std::fs::read(self.cache_dir().join(&md.id)).map_err(|e| format!("Its SKILL.md could not be read: {e}"))?;
        let manifest = skills::parse(&String::from_utf8_lossy(&text), name)?;
        if manifest.name != name {
            return Err(format!("Its SKILL.md is named '{}', but the skill '{name}': make them the same in Wysebee.", manifest.name));
        }
        // Checked again here: the list may have changed while the files came.
        if self.0.store.get().skill(name).as_ref() != Some(&entry) {
            return Err(format!("'{name}' changed in Wysebee meanwhile. Look at it again, then allow it."));
        }
        let folder = self.skills_dir().join(name);
        let executable: HashSet<String> = manifest.scripts.iter().filter_map(|s| skills::local_program(&s.command)).collect();
        skills::place(&folder, &self.cache_dir(), &entry.files, state.placed(name), &executable)?;
        let mut scripts = vec![];
        for s in &manifest.scripts {
            let words = skills::split_command(&s.command).unwrap_or_default();
            let first = words.first().map(String::as_str).unwrap_or("");
            match skills::program(&folder, first, self.0.opts.path.as_deref()) {
                Ok(program) => scripts.push(ApprovedScript { name: s.name.clone(), command: s.command.clone(), description: s.description.clone(), program, in_folder: first.contains('/') }),
                Err(why) => {
                    // The files are in place, and nothing of it is allowed.
                    self.0.store.update(|st| st.record_placed(name, entry.files.clone()));
                    self.0.kick.notify_one();
                    return Err(format!("Script '{}': {why}", s.name));
                }
            }
        }
        self.0.store.update(|st| st.record_skill(ApprovedSkill { name: name.to_string(), files: entry.files.clone(), scripts }));
        tracing::info!(skill = name, "skill_allowed");
        self.0.kick.notify_one();
        Ok(())
    }

    /// The person took the skill `name`'s approval back. Its folder stays.
    pub fn revoke_skill(&self, name: &str) {
        self.0.store.update(|s| s.revoke_skill(name));
        self.0.rt.lock().unwrap().seen_waiting = waiting_keys(&self.0.store.get());
        self.0.kick.notify_one();
    }

    /// The skill `name`'s folder on this PC, made when it is not there yet: the person may
    /// put files there (they stay on the PC and are never sent). Only for a skill the agent
    /// has, or had.
    pub fn skill_folder(&self, name: &str) -> Result<PathBuf, String> {
        let state = self.0.store.get();
        let known = state.skill(name).is_some() || state.placed_skills.iter().any(|p| p.name == name);
        if !skills::valid_name(name) || !known {
            return Err(format!("There is no skill '{name}' here."));
        }
        let folder = self.skills_dir().join(name);
        std::fs::create_dir_all(&folder).map_err(|e| format!("The skill's folder could not be made: {e}"))?;
        Ok(folder)
    }

    /// The person picked a folder for the agent (in the window, a native folder picker):
    /// `name` ("" for the folder's own), `mode` `read` or `write`, a `description`. Its id.
    pub fn add_folder(&self, path: &str, name: &str, mode: &str, description: &str) -> Result<String, String> {
        if !self.0.store.get().link.is_some_and(|l| l.agent.is_some()) {
            return Err("This PC is not paired.".into());
        }
        let id = self.0.store.update(|s| {
            let folder = folders::new_folder(&s.folders, path, name, mode, description)?;
            let id = folder.id.clone();
            s.folders.push(folder);
            Ok::<_, String>(id)
        })?;
        tracing::info!(folder = %id, "folder_added");
        self.0.kick.notify_one();
        Ok(id)
    }

    /// The person changed a folder's name, permission or description (its path stays).
    pub fn update_folder(&self, id: &str, name: &str, mode: &str, description: &str) -> Result<(), String> {
        self.0.store.update(|s| {
            let f: &mut Folder = s.folders.iter_mut().find(|f| f.id == id).ok_or(format!("There is no folder '{id}' here."))?;
            let default = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| f.path.display().to_string());
            let (name, mode, description) = (folders::checked_name(name, &default)?, Mode::parse(mode)?, folders::checked_description(description)?);
            (f.name, f.mode, f.description) = (name, mode, description);
            Ok::<_, String>(())
        })?;
        self.0.kick.notify_one();
        Ok(())
    }

    /// The person removed a folder: the agent can no longer reach it. Its id is not used again.
    pub fn remove_folder(&self, id: &str) -> Result<(), String> {
        self.0.store.update(|s| {
            let before = s.folders.len();
            s.folders.retain(|f| f.id != id);
            if s.folders.len() == before {
                return Err(format!("There is no folder '{id}' here."));
            }
            Ok(())
        })?;
        tracing::info!(folder = id, "folder_removed");
        self.0.kick.notify_one();
        Ok(())
    }

    /// "Let Wysebee run programs on this PC": off pauses. Pausing stops what runs now,
    /// and answers those calls as paused.
    pub async fn set_paused(&self, paused: bool) {
        self.0.store.update(|s| s.paused = paused);
        if paused {
            self.0.jobs.stop_all();
            let stopped: Vec<String> = {
                let mut rt = self.0.rt.lock().unwrap();
                rt.running.drain().map(|(id, running)| (id, running.abort()).0).collect()
            };
            if let (Some(link), Ok(Some(secret))) = (self.0.store.get().link, self.secret()) {
                for id in stopped {
                    self.post_result(&link, &secret, &id, &json!({"ok": false, "error": PAUSED})).await;
                }
            }
        }
        self.0.kick.notify_one();
    }

    /// "Let Wysebee run any program on this PC", on or off: the cloud learns at once. Turned
    /// off, a `program_run` already running goes on to its end (as an allowed program does when
    /// its approval is taken back); the next one is refused.
    pub fn set_any_program(&self, on: bool) {
        self.0.store.update(|s| s.any_program = on);
        tracing::info!(on, "any_program_set");
        self.0.kick.notify_one();
    }

    /// "Let Wysebee use this PC as a developer", on or off: the cloud learns at once. Turned
    /// off, every background job is stopped.
    pub fn set_dev_access(&self, on: bool) {
        self.0.store.update(|s| s.dev_access = on);
        if !on {
            self.0.jobs.stop_all();
        }
        tracing::info!(on, "dev_access_set");
        self.0.kick.notify_one();
    }

    /// "Let Wysebee use Android phones on this PC", on or off: the cloud learns at once.
    pub fn set_android_dev(&self, on: bool) {
        self.0.store.update(|s| s.android_dev = on);
        tracing::info!(on, "android_dev_set");
        self.0.kick.notify_one();
    }

    /// "Let Wysebee see and control this PC's screen", on or off: the cloud learns at once.
    pub fn set_screen_control(&self, on: bool) {
        self.0.store.update(|s| s.screen_control = on);
        tracing::info!(on, "screen_control_set");
        self.0.kick.notify_one();
    }

    /// Save the cloud's address from the window ("" for the default). Only while
    /// unpaired: a link keeps the cloud it was made with.
    pub fn set_cloud_url(&self, url: &str) -> Result<(), String> {
        if self.0.store.get().link.is_some() {
            return Err("Unpair first: the address is used when this PC is paired.".into());
        }
        let saved = if url.trim().is_empty() { None } else { Some(cloud::checked(url)?) };
        self.0.store.update(|s| s.cloud_url = saved);
        Ok(())
    }

    /// What the poll says of this PC.
    fn poll_status(&self) -> Value {
        let state = self.0.store.get();
        let approved: Vec<String> = state.allowed_names().into_iter().take(link::MAX_APPROVED).collect();
        let approved_skills: Vec<String> = state.allowed_skill_names().into_iter().take(link::MAX_APPROVED_SKILLS).collect();
        let folders: Vec<Value> = state.folders.iter().take(folders::MAX_FOLDERS).map(Folder::reported).collect();
        json!({"paused": state.paused, "extension_version": self.0.opts.version, "browser": link::device_name(&self.0.opts.device_name), "approved": approved, "approved_skills": approved_skills, "folders": folders, "any_program": state.any_program, "screen_control": state.screen_control, "dev_access": state.dev_access, "android_dev": state.android_dev})
    }

    /// One poll, and what it brought. Cut short and asked again when the status changes
    /// meanwhile (a program allowed, paused), so the cloud learns at once.
    pub async fn step(&self) -> Step {
        loop {
            let Some(link) = self.0.store.get().link else { return Step::Idle };
            let secret = match self.secret() {
                Ok(Some(secret)) => secret,
                Ok(None) => {
                    self.forget(Some("This PC's pairing key is no longer in the Credential Manager. Pair it again."));
                    return Step::Unpaired;
                }
                Err(e) => {
                    self.0.rt.lock().unwrap().connected = false;
                    return Step::Failed(e);
                }
            };
            let t = &self.0.opts.timing;
            let status = self.poll_status();
            let answer = tokio::select! {
                answer = self.0.client.poll(&link.cloud_url, &link.id, &secret, &status, t.wait_secs, Duration::from_secs(t.wait_secs) + t.grace) => answer,
                _ = self.0.kick.notified() => continue,
            };
            // Unpaired or cancelled here while the cloud was asked: the answer is nobody's.
            if self.0.store.get().link.map(|l| l.id) != Some(link.id.clone()) {
                return Step::Idle;
            }
            let step = self.took(&link, &secret, answer);
            if matches!(step, Step::Paired { .. }) {
                self.store_skills(link, secret);
            }
            return step;
        }
    }

    fn took(&self, link: &Link, secret: &str, answer: Result<Poll, Failure>) -> Step {
        let answer = match answer {
            Ok(answer) => answer,
            Err(f) => {
                self.0.rt.lock().unwrap().connected = false;
                return Step::Failed(f.message().to_string());
            }
        };
        match answer {
            Poll::Unpaired => {
                tracing::info!("unpaired_by_cloud");
                self.forget(Some("This PC was unpaired in Wysebee, or its agent was deleted."));
                Step::Unpaired
            }
            Poll::Expired => {
                self.forget(Some("The code ran out before it was typed. Start again."));
                Step::Expired
            }
            Poll::Refused { message } => {
                self.forget(None);
                self.0.rt.lock().unwrap().refused = Some(message);
                Step::Refused
            }
            Poll::Pending { expires_in } => {
                self.0.rt.lock().unwrap().connected = true;
                if let Some(left) = expires_in {
                    let at = now() + left;
                    self.0.store.update(|s| {
                        // Only a real change is written: the cloud's count and this clock differ by a second or two.
                        if let Some(l) = s.link.as_mut().filter(|l| l.expires_at.is_none_or(|was| was.abs_diff(at) > 5)) {
                            l.expires_at = Some(at);
                        }
                    });
                }
                Step::Pending
            }
            Poll::Paired { name, label, programs, skills, calls, cancelled } => {
                let state = self.0.store.update(|s| {
                    if let Some(l) = s.link.as_mut() {
                        l.agent = Some(AgentName { name, label });
                        l.code = None;
                        l.expires_at = None;
                    }
                    if let Some(list) = &programs {
                        s.sync(list);
                    }
                    if let Some(list) = &skills {
                        s.sync_skills(list);
                    }
                    s.clone()
                });
                let look = {
                    let mut rt = self.0.rt.lock().unwrap();
                    rt.connected = true;
                    for id in &cancelled {
                        if let Some(running) = rt.running.remove(id) {
                            tracing::info!(call = %id, "call_cancelled");
                            running.abort();
                        }
                    }
                    let waiting = waiting_keys(&state);
                    let look = waiting.iter().any(|k| !rt.seen_waiting.contains(k));
                    rt.seen_waiting = waiting;
                    look
                };
                if look {
                    if let Some(hook) = self.0.attention.lock().unwrap().clone() {
                        hook();
                    }
                }
                let count = calls.len();
                for call in calls {
                    self.start_call(link.clone(), secret.to_string(), call);
                }
                Step::Paired { calls: count }
            }
        }
    }

    /// Run a call beside the loop, so a cancel can reach it while it runs.
    fn start_call(&self, link: Link, secret: String, call: Call) {
        let me = self.clone();
        let id = call.id.clone();
        // Held across the spawn: the task takes itself off the list only once it is on it.
        let mut rt = self.0.rt.lock().unwrap();
        let task = tokio::spawn(async move {
            let body = match me.perform(&call.op, &call.args).await {
                Ok(result) => json!({"ok": true, "result": result}),
                Err(error) => json!({"ok": false, "error": error}),
            };
            me.post_result(&link, &secret, &call.id, &body).await;
            me.0.rt.lock().unwrap().running.remove(&call.id);
        });
        rt.running.insert(id, task.abort_handle());
    }

    async fn post_result(&self, link: &Link, secret: &str, call_id: &str, body: &Value) {
        let t = &self.0.opts.timing;
        for attempt in 0..3 {
            match self.0.client.result(&link.cloud_url, &link.id, secret, call_id, body, t.quick).await {
                Ok(()) => return,
                Err(f) => tracing::warn!(call = %call_id, attempt, error = %f.message(), "result_not_sent"),
            }
            tokio::time::sleep(t.min_gap).await;
        }
    }

    /// One call: `ping`, `local_run`, `skill_run`, `folder_call` or `program_run`. Anything
    /// else is refused.
    pub async fn perform(&self, op: &str, args: &Value) -> Result<Value, String> {
        match op {
            "ping" => Ok(json!({"pong": true})),
            "local_run" => self.local_run(args).await,
            "program_run" => self.program_run(args).await,
            "screen_screenshot" => self.screen_call(None).await,
            "dev_shell" => self.dev_shell(args).await,
            "dev_job" => self.dev_job(args),
            "dev_file" => self.dev_file(args).await,
            "android_dev" => self.android_dev(args).await,
            "screen_action" => self.screen_call(Some(args.clone())).await,
            "skill_run" => self.skill_run(args).await,
            "folder_call" => self.folder_call(args).await,
            other => Err(format!("Wysebee Agent for Windows cannot do '{other}'.")),
        }
    }

    /// `local_run`: the approved copy of the named program, with the given values in its
    /// blanks. Nothing of the call but those values reaches the argument list.
    async fn local_run(&self, args: &Value) -> Result<Value, String> {
        let name = args.get("name").and_then(Value::as_str).unwrap_or("");
        let values = args.get("values").cloned().unwrap_or(Value::Null);
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        let program = state.program(name).ok_or(format!("This agent has no program '{name}'."))?;
        let approved = state.runnable(name).ok_or(format!("'{name}' is waiting for approval on the PC. The person can allow it in Wysebee Agent, on that PC."))?;
        let argv = programs::fill(&approved.args, &values)?;
        if !approved.path.is_file() {
            return Err(format!("The program '{}' is no longer on the PC (it was {}). The person can allow it again in Wysebee Agent once it is back, or change the tool in Wysebee.", program.command, approved.path.display()));
        }
        let limit = self.0.opts.timing.time_limit;
        tracing::info!(program = name, "program_started");
        match programs::execute(&approved.path, &argv, &self.0.opts.home, limit).await {
            Err(e) => Err(format!("'{}' could not be started: {e}", program.command)),
            Ok(None) => Err(format!("'{}' did not finish within {} seconds and was stopped.", program.command, limit.as_secs())),
            Ok(Some(out)) => Ok(programs::answer(&out)),
        }
    }

    /// `program_run`: the program the AI names, with its
    /// arguments — only while the person has turned on "Let Wysebee run any program on this
    /// PC" here. Found as an allowed program is (an `.exe`/`.com` on the `PATH` or by its full
    /// path; never a batch file), run as one is: an argument list, never a shell, no input,
    /// in the person's home folder, for at most the time limit, output cut.
    async fn program_run(&self, args: &Value) -> Result<Value, String> {
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        if !state.any_program {
            return Err(ANY_OFF.into());
        }
        let command = args.get("program").and_then(Value::as_str).unwrap_or("").trim();
        if command.is_empty() || command.chars().count() > programs::MAX_TEXT {
            return Err("Name the program to run.".into());
        }
        let argv: Vec<String> = match args.get("args") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) if a.len() <= programs::MAX_ARGS => a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect(),
            Some(_) => return Err(format!("'args' is a list of at most {} arguments.", programs::MAX_ARGS)),
        };
        let path = self.0.opts.path.as_deref();
        let exe = programs::resolve(command, path).ok_or_else(|| programs::not_found(command, path))?;
        // What the AI may not run on its own (guard.rs): a safeguard, not a sandbox.
        if let Some(why) = crate::guard::refusal(&exe, &argv) {
            tracing::info!(program = %exe.display(), "any_program_refused");
            return Err(why);
        }
        let limit = self.0.opts.timing.time_limit;
        // The program's name only: never its arguments, which the AI chose.
        tracing::info!(program = %exe.display(), "any_program_started");
        match programs::execute(&exe, &argv, &self.0.opts.home, limit).await {
            Err(e) => Err(format!("'{command}' could not be started: {e}")),
            Ok(None) => Err(format!("'{command}' did not finish within {} seconds and was stopped.", limit.as_secs())),
            Ok(Some(out)) => Ok(programs::answer(&out)),
        }
    }

    /// Refused while paused, or while `on` (the person's switch) is off, with `off` said.
    fn may(&self, on: impl Fn(&crate::approvals::State) -> bool, off: &str) -> Result<(), String> {
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        if !on(&state) {
            return Err(off.into());
        }
        Ok(())
    }

    /// `dev_shell`: a command through PowerShell (or pwsh, or cmd), in the folder it names —
    /// waited for (at most 220 s), or started as a background job. The command itself is never
    /// logged.
    async fn dev_shell(&self, args: &Value) -> Result<Value, String> {
        self.may(|s| s.dev_access, DEV_OFF)?;
        let ask = crate::dev::ask(args, &self.0.opts.home)?;
        let (name, _, _) = crate::dev::shell_of(&ask.shell)?;
        let path = self.0.opts.path.as_deref();
        let exe = programs::resolve(name, path).ok_or_else(|| programs::not_found(name, path))?;
        tracing::info!(shell = name, background = ask.background, "dev_shell");
        let child = crate::dev::command(&exe, &ask).spawn().map_err(|e| format!("{name} could not be started: {e}"))?;
        if ask.background {
            let job = self.0.jobs.start(&ask.command, child)?;
            return Ok(json!({"job": job, "status": "running", "note": "Started in the background: read its output with dev_job."}));
        }
        // Waited for; past the limit the wait is dropped, and the shell killed with it.
        match tokio::time::timeout(ask.limit, child.wait_with_output()).await {
            Err(_) => Err(format!("The command did not finish within {} seconds and was stopped. Run a longer one in the background (\"background\": true).", ask.limit.as_secs())),
            Ok(out) => out.map(|o| crate::dev::answer(&o)).map_err(|e| format!("The command could not be waited for: {e}")),
        }
    }

    /// `dev_job`: a background job's output and state (`output`, its newest characters), stop
    /// it (`stop`), or every job (`list`).
    fn dev_job(&self, args: &Value) -> Result<Value, String> {
        self.may(|s| s.dev_access, DEV_OFF)?;
        let action = args.get("action").and_then(Value::as_str).unwrap_or("output");
        if action == "list" {
            return Ok(json!({"jobs": self.0.jobs.list()}));
        }
        let id = args.get("job").and_then(Value::as_str).unwrap_or("");
        let job = self.0.jobs.get(id).ok_or(format!("There is no job '{id}': list them with action list."))?;
        match action {
            "output" => {
                let tail = args.get("tail").and_then(Value::as_u64).map_or(crate::dev::MAX_OUT, |t| (t as usize).clamp(1, crate::dev::MAX_OUT));
                Ok(job.report(id, tail))
            }
            "stop" => {
                let stopped = self.0.jobs.stop(id);
                Ok(json!({"job": id, "stopped": stopped}))
            }
            other => Err(format!("'{other}' is not an action: output, stop or list.")),
        }
    }

    /// `dev_file`: read, write, edit, glob, grep or list, anywhere (on a blocking thread).
    async fn dev_file(&self, args: &Value) -> Result<Value, String> {
        self.may(|s| s.dev_access, DEV_OFF)?;
        let (home, args) = (self.0.opts.home.clone(), args.clone());
        let tool = args.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
        tracing::info!(tool = %tool, "dev_file");
        tokio::task::spawn_blocking(move || crate::dev::file(&home, &args)).await.map_err(|e| format!("The file tool stopped: {e}"))?
    }

    /// `android_dev`: one of the Android toolkit's tools `{tool, args}`, through adb.
    async fn android_dev(&self, args: &Value) -> Result<Value, String> {
        self.may(|s| s.android_dev, ANDROID_OFF)?;
        let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
        let inner = args.get("args").cloned().unwrap_or_else(|| json!({}));
        let exe = programs::resolve("adb", self.0.opts.path.as_deref()).ok_or(crate::android::NO_ADB)?;
        tracing::info!(tool, "android_dev");
        let adb = move |a: Vec<String>, t: Duration| -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<crate::android::Out, String>> + Send>> { Box::pin(crate::android::spawn(exe.clone(), a, t)) };
        crate::android::call(tool, &inner, &self.0.opts.home, &adb).await
    }

    /// `screen_screenshot` (`action` None) or `screen_action`: only while the person has turned
    /// on "Let Wysebee see and control this PC's screen". An action is done, the screen let
    /// settle, and its new screenshot answered (unless `"screenshot": false`). The work runs on a
    /// blocking thread; typed text is never logged.
    async fn screen_call(&self, action: Option<Value>) -> Result<Value, String> {
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        if !state.screen_control {
            return Err(SCREEN_OFF.into());
        }
        let screen = self.0.opts.screen.clone().ok_or("This PC has no screen Wysebee Agent can show.")?;
        let settle = self.0.opts.timing.settle;
        let work = move || -> Result<Value, String> {
            let Some(args) = action else {
                return crate::screen::picture(&screen.capture()?);
            };
            let done = crate::screen::parse(&args, screen.size()?)?;
            screen.act(&done)?;
            let what = args.get("action").and_then(|a| a.as_str()).unwrap_or_default().to_string();
            tracing::info!(action = %what, "screen_action");
            let mut answer = json!({"done": done.said()});
            if args.get("screenshot") != Some(&Value::Bool(false)) {
                std::thread::sleep(settle);
                let shot = crate::screen::picture(&screen.capture()?)?;
                answer.as_object_mut().unwrap().extend(shot.as_object().unwrap().clone());
            }
            Ok(answer)
        };
        tokio::task::spawn_blocking(work).await.map_err(|e| format!("The screen could not be used: {e}"))?
    }

    /// `skill_run`: a script the approved copy of the named skill declares, from its folder,
    /// with the call's arguments after the declared command's words. Nothing else of the
    /// call reaches the argument list.
    async fn skill_run(&self, args: &Value) -> Result<Value, String> {
        let name = args.get("skill").and_then(Value::as_str).unwrap_or("");
        let wanted = args.get("script").and_then(Value::as_str).unwrap_or("");
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        state.skill(name).ok_or(format!("This agent has no skill '{name}'."))?;
        let approved = state.runnable_skill(name).ok_or(format!("The skill '{name}' is waiting for approval on the PC. The person can allow it in Wysebee Agent, on that PC."))?;
        let script = approved.scripts.iter().find(|s| s.name == wanted).ok_or(format!("The skill '{name}' has no script '{wanted}'."))?;
        let extra: Vec<String> = match args.get("args") {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) if a.len() <= skills::MAX_ARGS => a.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect(),
            Some(_) => return Err(format!("'args' is a list of at most {} arguments.", skills::MAX_ARGS)),
        };
        let folder = self.skills_dir().join(name);
        if !still_there(script, &folder) {
            return Err(format!("The program of '{}' is no longer where it was ({}). The person can allow the skill '{name}' again in Wysebee Agent.", script.name, script.program.display()));
        }
        let words = skills::split_command(&script.command).unwrap_or_default();
        let argv: Vec<String> = words.into_iter().skip(1).chain(extra).collect();
        let limit = self.0.opts.timing.time_limit;
        tracing::info!(skill = name, script = wanted, "script_started");
        match programs::execute(&script.program, &argv, &folder, limit).await {
            Err(e) => Err(format!("'{}' could not be started: {e}", script.command)),
            Ok(None) => Err(format!("'{}' did not finish within {} seconds and was stopped.", script.name, limit.as_secs())),
            Ok(Some(out)) => Ok(programs::answer(&out)),
        }
    }

    /// `folder_call`: one folder tool in one of the folders the person picked here (by its
    /// id; none: `folder_find` across them all), with the permission they gave it.
    async fn folder_call(&self, args: &Value) -> Result<Value, String> {
        let state = self.0.store.get();
        if state.paused {
            return Err(PAUSED.into());
        }
        let tool = args.get("tool").and_then(Value::as_str).unwrap_or("").to_string();
        let folder = match args.get("folder") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return Err("'folder' is a folder's id, or null.".into()),
        };
        let inner = match args.get("args") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return Err("'args' is an object.".into()),
        };
        tracing::info!(tool = %tool, "folder_tool_started");
        // File work blocks: off the async threads.
        tokio::task::spawn_blocking(move || folders::call(&state.folders, &tool, folder.as_deref(), &inner)).await.map_err(|e| format!("The folder tool stopped: {e}"))?
    }

    /// How many calls run now.
    pub fn running(&self) -> usize {
        self.0.rt.lock().unwrap().running.len()
    }

    /// The link's loop, for as long as the app runs: poll, run what is handed out, back
    /// off 2, 5, 15, then 30 s while the cloud cannot be reached.
    pub async fn run(&self) {
        let t = &self.0.opts.timing;
        let mut failures = 0usize;
        loop {
            let began = Instant::now();
            match self.step().await {
                Step::Idle => {
                    failures = 0;
                    self.0.kick.notified().await;
                }
                Step::Failed(error) => {
                    let wait = t.backoff_after(failures);
                    failures += 1;
                    tracing::warn!(%error, failures, "poll_failed");
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = self.0.kick.notified() => {}
                    }
                }
                Step::Pending | Step::Paired { calls: 0 } => {
                    failures = 0;
                    let gap = t.min_gap.saturating_sub(began.elapsed());
                    if !gap.is_zero() {
                        tokio::time::sleep(gap).await;
                    }
                }
                Step::Paired { .. } | Step::Refused | Step::Unpaired | Step::Expired => failures = 0,
            }
        }
    }
}

/// Whether an approved script's program is still a file — and, for one of the skill's own
/// files, still inside the skill's folder once links are followed.
fn still_there(script: &ApprovedScript, folder: &Path) -> bool {
    if !script.program.is_file() {
        return false;
    }
    if !script.in_folder {
        return true;
    }
    // Where it is, and where the folder is, must both be made out (a path too long, a file
    // held by another program: not known to be inside).
    dunce::canonicalize(&script.program).ok().zip(dunce::canonicalize(folder).ok()).is_some_and(|(real, root)| real.starts_with(root))
}

/// The one instance of the app for a state folder: the lock is held while the returned
/// file lives; None when another instance holds it (two would take each other's calls).
pub fn single_instance(state_dir: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(state_dir).ok()?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(state_dir.join("lock")).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// The file the running instance leaves in its state folder for a second launch: the
/// loopback port it listens on, then a token.
const WAKE_FILE: &str = "wake";
/// How long either side of a wake waits on the other.
const WAKE_WAIT: Duration = Duration::from_secs(2);

/// Listen for a second launch on this state folder ([`wake_running`]) and call `on_wake`
/// for each, on a thread of its own, for as long as the process runs. Called by the
/// instance holding [`single_instance`]'s lock, so a person who opens the app again while
/// it sits in the menu bar or the notification area sees its window rather than nothing.
///
/// A port on 127.0.0.1 and a random token, both written to `wake` (this account's only):
/// a process that cannot read the file cannot ask, and asking only shows the window.
pub fn listen_for_wake(state_dir: &Path, on_wake: impl Fn() + Send + 'static) -> std::io::Result<()> {
    use std::io::Write;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let token = folders::new_id();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(state_dir.join(WAKE_FILE))?.write_all(format!("{} {token}", listener.local_addr()?.port()).as_bytes())?;
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            if heard(&stream, &token) {
                on_wake();
                let _ = stream.write_all(b"ok");
            }
        }
    });
    Ok(())
}

/// One knock on the wake port: the token and nothing else, within [`WAKE_WAIT`].
fn heard(stream: &std::net::TcpStream, token: &str) -> bool {
    use std::io::Read;
    let mut got = String::new();
    stream.set_read_timeout(Some(WAKE_WAIT)).is_ok() && stream.take(64).read_to_string(&mut got).is_ok() && got == token
}

/// Ask the instance running on this state folder to bring its window forward: true when it
/// answered, false when none did (it is still starting, or it is a version that does not
/// listen).
pub fn wake_running(state_dir: &Path) -> bool {
    knock(state_dir).unwrap_or(false)
}

fn knock(state_dir: &Path) -> Option<bool> {
    use std::io::{Read, Write};
    let text = std::fs::read_to_string(state_dir.join(WAKE_FILE)).ok()?;
    let (port, token) = text.trim().split_once(' ')?;
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port.parse::<u16>().ok()?));
    let mut stream = std::net::TcpStream::connect_timeout(&address, WAKE_WAIT).ok()?;
    stream.set_read_timeout(Some(WAKE_WAIT)).ok()?;
    stream.write_all(token.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut answer = String::new();
    stream.take(8).read_to_string(&mut answer).ok()?;
    Some(answer == "ok")
}
