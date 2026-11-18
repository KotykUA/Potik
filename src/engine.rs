use crate::aria::Aria;
use crate::checkpoint::{Checkpoints, sync_download, write_atomic};
use crate::probe::{HttpOptions, Login, RangeSupport};
use crate::process::random_hex;
use crate::verification::{self, Checks, Report};
use gtk::glib;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub const CONNECTION_RANGE: std::ops::RangeInclusive<u8> = 1..=32;
const CONNECTIONS_PER_HOST: u8 = 16;
const LOGIN_REQUIRED: &str = "Введіть дані HTTP-авторизації, щоб продовжити завантаження.";
const LOGIN_REJECTED: &str =
    "Сервер вимагає HTTP-авторизації. Перевірте ім’я користувача й пароль.";
const REFRESH: Duration = Duration::from_millis(650);
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
const KEYS: &[&str] = &[
    "gid",
    "status",
    "dir",
    "totalLength",
    "completedLength",
    "downloadSpeed",
    "files",
    "errorCode",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Waiting,
    Paused,
    Verifying,
    Complete,
    Error,
    Removed,
}

impl Status {
    pub fn is_live(self) -> bool {
        matches!(self, Self::Active | Self::Waiting | Self::Paused)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Spec {
    pub url: String,
    pub dir: String,
    pub name: String,
    #[serde(default = "default_connections")]
    pub connections: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    mirrors: Vec<String>,
    #[serde(default)]
    max_speed_kib: u32,
    #[serde(default)]
    range_unavailable: bool,
    #[serde(default, skip_serializing_if = "HttpOptions::is_empty")]
    http: HttpOptions,
    #[serde(default)]
    needs_login: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume: Option<ExistingFile>,
    #[serde(default, skip_serializing_if = "Checks::is_empty")]
    checks: Checks,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExistingFile {
    remote_length: u64,
    initial_length: Option<u64>,
}

impl Spec {
    pub fn new(url: &str, dir: &Path, name: &str, connections: u8) -> Result<Self> {
        let url = url.trim();
        let name = name.trim();
        validate_url(url)?;
        if name == "."
            || name == ".."
            || name.contains(['/', '\\'])
            || name.chars().any(char::is_control)
        {
            return Err("Введіть лише назву файлу без шляху.".into());
        }
        let dir = download_directory(dir)?;
        validate_connections(connections)?;
        Ok(Self {
            url: url.into(),
            dir: dir.into(),
            name: name.into(),
            connections,
            mirrors: Vec::new(),
            max_speed_kib: 0,
            range_unavailable: false,
            http: HttpOptions::default(),
            needs_login: false,
            resume: None,
            checks: Checks::default(),
        })
    }

    pub fn resume_existing(mut self, remote_length: u64) -> Result<Self> {
        if self.name.is_empty() {
            return Err("Вкажіть точну назву частково завантаженого файлу.".into());
        }
        self.resume = Some(ExistingFile {
            remote_length,
            initial_length: None,
        });
        Ok(self)
    }

    pub fn with_sha256(mut self, value: &str) -> Result<Self> {
        self.checks.sha256 = verification::sha256_value(value)?;
        Ok(self)
    }

    pub fn with_options(
        mut self,
        mirrors: &str,
        max_speed_kib: u32,
        http: HttpOptions,
        range_support: RangeSupport,
    ) -> Result<Self> {
        if url_has_login(&self.url) {
            return Err("Вводьте дані для входу окремо від посилання.".into());
        }
        for url in mirrors.lines().map(str::trim).filter(|url| !url.is_empty()) {
            if validate_url(url).is_err() || url_has_login(url) {
                return Err(
                    "Вкажіть дійсні HTTP або HTTPS-посилання на дзеркала без даних для входу."
                        .into(),
                );
            }
            if url != self.url && !self.mirrors.iter().any(|mirror| mirror == url) {
                self.mirrors.push(url.into());
            }
        }
        self.max_speed_kib = max_speed_kib;
        self.range_unavailable = matches!(range_support, RangeSupport::Unavailable);
        self.needs_login = http.login.is_some();
        self.http = http;
        Ok(self)
    }
}

pub(crate) fn url_has_login(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|host| host.contains('@'))
    })
}

pub fn validate_url(url: &str) -> std::result::Result<(), &'static str> {
    if url.chars().any(char::is_whitespace) || url.chars().any(char::is_control) {
        return Err("Вставте HTTP або HTTPS-посилання без пробілів.");
    }
    let invalid = "Вставте дійсне HTTP або HTTPS-посилання.";
    let uri = glib::Uri::parse(url, glib::UriFlags::NONE).map_err(|_| invalid)?;
    if !matches!(uri.scheme().as_str(), "http" | "https")
        || uri.host().is_none_or(|host| host.is_empty())
    {
        return Err(invalid);
    }
    Ok(())
}

pub fn default_connections() -> u8 {
    8
}

pub fn validate_connections(connections: u8) -> std::result::Result<(), &'static str> {
    if CONNECTION_RANGE.contains(&connections) {
        Ok(())
    } else {
        Err("Кількість потоків має бути від 1 до 32.")
    }
}

pub fn download_directory(path: &Path) -> std::result::Result<&str, &'static str> {
    if !path.is_absolute() {
        return Err("Оберіть локальну папку з абсолютним шляхом.");
    }
    path.to_str()
        .filter(|dir| !dir.contains(['\0', '\n', '\r']))
        .ok_or("Шлях до папки має бути у форматі UTF-8 без керівних символів.")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Download {
    pub gid: String,
    pub spec: Spec,
    pub status: Status,
    pub path: String,
    pub completed: u64,
    pub total: u64,
    #[serde(skip)]
    pub speed: u64,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Report>,
}

impl Download {
    pub fn requires_login_for_retry(&self) -> bool {
        self.verification.is_none()
            && (self.error.as_deref() == Some(LOGIN_REJECTED)
                || (self.spec.needs_login && self.spec.http.login.is_none()))
    }

    pub fn file_name(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|name| name.to_str())
            .or_else(|| (!self.spec.name.is_empty()).then_some(self.spec.name.as_str()))
            .or_else(|| {
                let url = self.spec.url.split(['?', '#']).next()?;
                url.split_once("://")?
                    .1
                    .split_once('/')?
                    .1
                    .rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or("Файл")
    }

    pub fn host(&self) -> String {
        glib::Uri::parse(&self.spec.url, glib::UriFlags::NONE)
            .ok()
            .and_then(|uri| uri.host())
            .map(|host| host.to_string())
            .unwrap_or_default()
    }
}

pub enum Action {
    Add {
        spec: Box<Spec>,
        paused: bool,
        signature: Option<PathBuf>,
    },
    Pause(String),
    Resume(String),
    Retry {
        gid: String,
        login: Option<Login>,
    },
    Remove(String),
    Reconnect,
    Shutdown,
}

pub enum Event {
    Snapshot(Vec<Download>),
    Offline(String),
    Notice(String),
}

pub fn spawn(state_dir: PathBuf) -> (Sender<Action>, Receiver<Event>, JoinHandle<()>) {
    let (send, requests) = mpsc::channel();
    let (events, receive) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut backend = connect(&state_dir, &events);
        let mut next_poll = Instant::now() + REFRESH;
        loop {
            match requests.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
                Ok(Action::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Action::Reconnect) => {
                    if let Some(mut engine) = backend.take() {
                        engine.shutdown();
                    }
                    backend = connect(&state_dir, &events);
                }
                Ok(action) => {
                    if let Some(engine) = &mut backend {
                        match engine.handle(action) {
                            Ok(()) => {
                                let _ = events.send(Event::Snapshot(engine.downloads.clone()));
                            }
                            Err(error) => {
                                let _ = events.send(Event::Notice(error.to_string()));
                            }
                        }
                    } else {
                        let _ = events.send(Event::Notice("aria2c наразі недоступний.".into()));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= next_poll {
                if let Some(engine) = &mut backend {
                    match engine.sync(false) {
                        Ok(()) => {
                            let _ = events.send(Event::Snapshot(engine.downloads.clone()));
                        }
                        Err(error) => {
                            let _ = events.send(Event::Offline(error.to_string()));
                        }
                    }
                }
                next_poll = Instant::now() + REFRESH;
            }
        }
        if let Some(engine) = &mut backend {
            engine.shutdown();
        }
    });
    (send, receive, worker)
}

fn connect(state_dir: &Path, events: &Sender<Event>) -> Option<Backend> {
    match Backend::start(state_dir) {
        Ok(backend) => {
            let _ = events.send(Event::Snapshot(backend.downloads.clone()));
            Some(backend)
        }
        Err(error) => {
            let _ = events.send(Event::Offline(error.to_string()));
            None
        }
    }
}

struct Backend {
    aria: Aria,
    downloads: Vec<Download>,
    directory: PathBuf,
    checkpoints: Checkpoints,
    verifier: Option<(String, JoinHandle<Report>)>,
    last_checkpoint: Instant,
    dirty: bool,
    _lock: File,
}

impl Backend {
    fn start(dir: &Path) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("instance.lock"))?;
        lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => "Потік уже працює з цією чергою.".into(),
            std::fs::TryLockError::Error(error) => format!("Не вдалося заблокувати чергу. {error}"),
        })?;
        let downloads = match fs::read(dir.join("downloads.json")) {
            Ok(data) => serde_json::from_slice(&data)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => vec![],
            Err(error) => return Err(error.into()),
        };
        let session = dir.join("queue.session");
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&session)?;
        let checkpoints = Checkpoints::new(dir)?;
        checkpoints.restore(&downloads);

        let mut backend = Self {
            aria: Aria::start(&session, default_connections())?,
            downloads,
            directory: dir.to_path_buf(),
            checkpoints,
            verifier: None,
            last_checkpoint: Instant::now(),
            dirty: false,
            _lock: lock,
        };
        backend.sync(true)?;
        Ok(backend)
    }

    fn shutdown(&mut self) {
        if let Err(error) = self.sync(false) {
            eprintln!("potik: {error}");
        }
        self.aria.stop();
        self.checkpoints.save(&self.downloads);
        if let Err(error) = self.sync_session() {
            eprintln!("potik: save queue: {error}");
        }
    }

    fn handle(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Add {
                mut spec,
                paused,
                signature,
            } => {
                if let Some(path) = signature {
                    spec.checks.signature = Some(
                        verification::read_signature(&path)
                            .map_err(|error| {
                                format!("Не вдалося прочитати підпис GPG/PGP. {error}")
                            })?
                            .into(),
                    );
                }
                self.add(*spec, paused)?;
            }
            Action::Retry { gid, login } => self.retry(&gid, login)?,
            Action::Pause(gid) => {
                self.aria.rpc.call("aria2.forcePause", vec![json!(gid)])?;
            }
            Action::Resume(gid) => {
                self.aria.rpc.call("aria2.unpause", vec![json!(gid)])?;
            }
            Action::Remove(gid) => {
                if self
                    .downloads
                    .iter()
                    .any(|item| item.gid == gid && item.status.is_live())
                {
                    self.aria.rpc.call("aria2.forceRemove", vec![json!(gid)])?;
                }
                self.downloads.retain(|item| item.gid != gid);
                self.checkpoints.discard(&gid);
                self.dirty = true;
            }
            Action::Reconnect | Action::Shutdown => unreachable!(),
        }
        self.flush()?;
        self.save_session()
    }

    fn retry(&mut self, gid: &str, login: Option<Login>) -> Result<()> {
        let index = self
            .downloads
            .iter()
            .position(|item| item.gid == gid && item.status == Status::Error)
            .ok_or("Це завантаження вже недоступне для повторної спроби.")?;
        let previous = &mut self.downloads[index];
        if previous.verification.take().is_some() {
            previous.status = Status::Verifying;
            previous.error = None;
            self.dirty = true;
            return Ok(());
        }

        let mut spec = previous.spec.clone();
        if let Some(login) = login {
            spec.needs_login = true;
            spec.http.login = Some(login);
        }
        if spec.needs_login && spec.http.login.is_none() {
            return Err(LOGIN_REQUIRED.into());
        }
        if !previous.path.is_empty() && control_file(Path::new(&previous.path)).is_file() {
            spec.name = previous.file_name().into();
        }
        self.add(spec, false)?;
        self.downloads.remove(index + 1);
        self.checkpoints.discard(gid);
        Ok(())
    }

    fn add(&mut self, spec: Spec, paused: bool) -> Result<()> {
        let item = self.prepare_download(spec, paused, random_hex::<8>()?)?;
        let persist_before_enqueue = !item.spec.checks.is_empty() || item.spec.needs_login;
        if !persist_before_enqueue {
            self.enqueue(&item)?;
        }
        self.downloads.insert(0, item);
        self.dirty = true;
        if persist_before_enqueue {
            // Preserve metadata that the durable aria2 session cannot recover.
            let result = self.flush().and_then(|_| self.enqueue(&self.downloads[0]));
            if let Err(error) = result {
                self.downloads[0].status = Status::Error;
                self.downloads[0].error = Some(error.to_string());
                self.dirty = true;
                self.flush()?;
                return Err(error);
            }
        }
        Ok(())
    }

    fn prepare_download(&self, mut spec: Spec, paused: bool, gid: String) -> Result<Download> {
        validate_connections(spec.connections)?;
        let (path, completed, total) = if let Some(resume) = &mut spec.resume {
            let path = Path::new(&spec.dir).join(&spec.name);
            let completed = self.prepare_resume(&path, resume)?;
            (
                path.to_string_lossy().into_owned(),
                completed,
                resume.remote_length,
            )
        } else {
            fs::create_dir_all(&spec.dir).map_err(|error| format!("Папка недоступна. {error}"))?;
            (String::new(), 0, 0)
        };
        Ok(Download {
            gid,
            spec,
            status: if paused {
                Status::Paused
            } else {
                Status::Waiting
            },
            path,
            completed,
            total,
            speed: 0,
            error: None,
            verification: None,
        })
    }

    fn enqueue(&self, item: &Download) -> Result<()> {
        let spec = &item.spec;
        let connections = if spec.range_unavailable {
            1
        } else {
            spec.connections
        };
        let mut options = json!({
            "gid": item.gid,
            "dir": spec.dir,
            "split": connections.to_string(),
            "max-connection-per-server": connections.min(CONNECTIONS_PER_HOST).to_string(),
            "max-download-limit": (u64::from(spec.max_speed_kib) * 1024).to_string(),
            "pause": if item.status == Status::Paused { "true" } else { "false" },
        });
        if !spec.name.is_empty() {
            options["out"] = json!(spec.name);
        }
        if !spec.http.user_agent.is_empty() {
            options["user-agent"] = json!(spec.http.user_agent);
        }
        if !spec.http.referer.is_empty() {
            options["referer"] = json!(spec.http.referer);
        }
        if spec.resume.is_some() {
            options["continue"] = json!("true");
            options["always-resume"] = json!("true");
            options["auto-file-renaming"] = json!("false");
            options["allow-overwrite"] = json!("false");
        }
        // Per-job http-user/http-passwd would also apply to mirrors and
        // redirects. Tie Basic credentials to the primary URL instead.
        let primary = match &spec.http.login {
            Some(login) => login.authenticate_url(&spec.url).ok_or(LOGIN_REQUIRED)?,
            None if spec.needs_login => return Err(LOGIN_REQUIRED.into()),
            None => spec.url.clone(),
        };
        let uris: Vec<_> = std::iter::once(primary)
            .chain(spec.mirrors.iter().cloned())
            .collect();
        let result = self
            .aria
            .rpc
            .call("aria2.addUri", vec![json!(uris), options]);
        if spec.needs_login {
            result.map_err(|_| "aria2c не вдалося додати посилання з HTTP-авторизацією.")?;
        } else {
            result?;
        }
        Ok(())
    }

    fn recover_download(&self, item: &Download) -> Result<Download> {
        let mut spec = item.spec.clone();
        if spec.needs_login && spec.http.login.is_none() {
            return Err(LOGIN_REQUIRED.into());
        }
        if !item.path.is_empty() {
            let path = Path::new(&item.path);
            if path.is_file() && !control_file(path).is_file() && spec.resume.is_none() {
                return Err("Точку докачування втрачено. Частковий файл збережено.".into());
            }
            if let Some(dir) = path.parent() {
                spec.dir = download_directory(dir)?.into();
                spec.name = item.file_name().into();
            }
        }
        let recovered =
            self.prepare_download(spec, item.status == Status::Paused, item.gid.clone())?;
        self.enqueue(&recovered)?;
        Ok(recovered)
    }

    fn prepare_resume(&self, path: &Path, resume: &mut ExistingFile) -> Result<u64> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("Не вдалося відкрити частковий файл. {error}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err("Оберіть наявний непорожній файл для докачування.".into());
        }
        let has_control = match fs::symlink_metadata(control_file(path)) {
            Ok(control) if control.is_file() => true,
            Ok(_) => return Err("Службовий файл .aria2 має бути звичайним файлом.".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let length = metadata.len();
        if length > resume.remote_length || (length == resume.remote_length && !has_control) {
            return Err("Наявний файл уже має розмір віддаленого файлу або більший.".into());
        }
        if !has_control
            && resume
                .initial_length
                .is_some_and(|initial| initial != length)
        {
            return Err(
                "Частковий файл змінився. Для безпечного відновлення потрібен файл .aria2.".into(),
            );
        }
        let destination = fs::canonicalize(path)?;
        let already_queued = self.downloads.iter().any(|item| {
            if !item.status.is_live() {
                return false;
            }
            let path = if item.path.is_empty() {
                Path::new(&item.spec.dir).join(item.file_name())
            } else {
                PathBuf::from(&item.path)
            };
            fs::canonicalize(path).is_ok_and(|path| path == destination)
        });
        if already_queued {
            return Err("Цей файл уже є серед активних завантажень або в черзі.".into());
        }
        resume.initial_length.get_or_insert(length);
        // The on-disk length may include gaps when aria2 has a piece map.
        Ok(if has_control { 0 } else { length })
    }

    fn sync(&mut self, startup: bool) -> Result<()> {
        self.collect_verification();
        let count = self.downloads.len().saturating_add(1024);
        let reports = self.aria.rpc.batch(vec![
            ("aria2.tellActive", vec![json!(KEYS)]),
            (
                "aria2.tellWaiting",
                vec![json!(0), json!(count), json!(KEYS)],
            ),
            (
                "aria2.tellStopped",
                vec![json!(0), json!(1000), json!(KEYS)],
            ),
        ])?;
        let mut reports_by_id = HashMap::new();
        for report in &reports {
            for item in report
                .as_array()
                .ok_or("aria2 повернула некоректний список.")?
            {
                if let Some(gid) = item["gid"].as_str() {
                    reports_by_id.insert(gid, item);
                }
            }
        }

        let mut missing = Vec::new();
        for (index, item) in self.downloads.iter_mut().enumerate() {
            if let Some(report) = reports_by_id.remove(item.gid.as_str()) {
                self.dirty |= apply_report(item, report)?;
            } else if startup && item.status.is_live() {
                missing.push(index);
            }
        }
        for index in missing {
            let item = &self.downloads[index];
            match self.recover_download(item) {
                Ok(mut recovered) => {
                    recovered.path.clone_from(&item.path);
                    recovered.completed = item.completed;
                    recovered.total = item.total;
                    self.downloads[index] = recovered;
                }
                Err(error) => {
                    self.downloads[index].status = Status::Error;
                    self.downloads[index].error = Some(error.to_string());
                }
            }
            self.dirty = true;
        }
        // Recover queue entries saved by aria2 before the app could write its metadata.
        if startup {
            for (gid, report) in reports_by_id {
                let url = report["files"][0]["uris"][0]["uri"]
                    .as_str()
                    .unwrap_or_default();
                let dir = report["dir"].as_str().unwrap_or_default();
                if url.is_empty() || dir.is_empty() {
                    continue;
                }
                let mut item = Download {
                    gid: gid.into(),
                    spec: Spec {
                        url: url.into(),
                        dir: dir.into(),
                        name: String::new(),
                        mirrors: Vec::new(),
                        max_speed_kib: 0,
                        range_unavailable: false,
                        http: HttpOptions::default(),
                        needs_login: false,
                        resume: None,
                        checks: Checks::default(),
                        connections: self
                            .aria
                            .rpc
                            .call("aria2.getOption", vec![json!(gid)])
                            .ok()
                            .and_then(|options| options["split"].as_str()?.parse().ok())
                            .filter(|count| CONNECTION_RANGE.contains(count))
                            .unwrap_or_else(default_connections),
                    },
                    status: Status::Waiting,
                    path: String::new(),
                    completed: 0,
                    total: 0,
                    speed: 0,
                    error: None,
                    verification: None,
                };
                apply_report(&mut item, report)?;
                self.downloads.push(item);
                self.dirty = true;
            }
        }
        let removed = self.downloads.len();
        self.downloads.retain(|item| item.status != Status::Removed);
        self.dirty |= self.downloads.len() != removed;
        self.flush()?;
        // Finished records now live in downloads.json. Prune them from aria2 so
        // it never writes failed records back to the unfinished queue.
        let mut session_changed = startup;
        if let Some(stopped) = reports[2].as_array() {
            for item in stopped {
                if let (Some(gid), Some("complete" | "removed")) =
                    (item["gid"].as_str(), item["status"].as_str())
                {
                    self.checkpoints.discard(gid);
                }
            }
            let calls: Vec<_> = stopped
                .iter()
                .filter_map(|item| item["gid"].as_str())
                .map(|gid| ("aria2.removeDownloadResult", vec![json!(gid)]))
                .collect();
            if !calls.is_empty() {
                self.aria.rpc.batch(calls)?;
                session_changed = true;
            }
        }
        if startup || self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            self.checkpoints.save(&self.downloads);
            self.last_checkpoint = Instant::now();
            session_changed |= self.downloads.iter().any(|item| item.status.is_live());
        }
        if session_changed {
            self.save_session()?;
        }
        self.start_verification();
        Ok(())
    }

    fn collect_verification(&mut self) {
        let Some((gid, worker)) = self.verifier.take_if(|(_, worker)| worker.is_finished()) else {
            return;
        };
        if let Some(item) = self
            .downloads
            .iter_mut()
            .find(|item| item.gid == gid && item.status == Status::Verifying)
        {
            let report = worker.join().unwrap_or_else(|_| Report {
                passed: false,
                message: "Не вдалося завершити перевірку файлу.".into(),
            });
            item.status = if report.passed {
                Status::Complete
            } else {
                Status::Error
            };
            item.verification = Some(report);
            self.dirty = true;
        }
    }

    fn start_verification(&mut self) {
        if self.verifier.is_none()
            && let Some(item) = self
                .downloads
                .iter()
                .find(|item| item.status == Status::Verifying)
        {
            let checks = item.spec.checks.clone();
            let path = PathBuf::from(&item.path);
            self.verifier = Some((item.gid.clone(), thread::spawn(move || checks.run(&path))));
        }
    }

    fn save_session(&self) -> Result<()> {
        self.aria.rpc.call("aria2.saveSession", vec![])?;
        self.sync_session()
    }

    fn sync_session(&self) -> Result<()> {
        let raw = fs::read_to_string(self.aria.session_path())?;
        write_atomic(&self.directory.join("queue.session"), |file| {
            let mut keep = true;
            for line in raw.split_inclusive('\n') {
                if !line.starts_with([' ', '\t']) {
                    keep = !line.trim_end().split('\t').any(url_has_login);
                }
                if keep {
                    file.write_all(line.as_bytes())?;
                }
            }
            Ok(())
        })
    }

    fn flush(&mut self) -> Result<()> {
        if self.dirty {
            write_atomic(&self.directory.join("downloads.json"), |file| {
                serde_json::to_writer(file, &self.downloads)?;
                Ok(())
            })?;
            self.dirty = false;
        }
        Ok(())
    }
}

fn apply_report(item: &mut Download, report: &Value) -> Result<bool> {
    let mut status: Status = serde_json::from_value(report["status"].clone())?;
    let path = report["files"][0]["path"].as_str().unwrap_or_default();
    if status == Status::Complete {
        if item.status == Status::Verifying || item.verification.is_some() {
            return Ok(false);
        }
        if item.status != Status::Complete && !path.is_empty() {
            sync_download(Path::new(path))?;
        }
        if !item.spec.checks.is_empty() {
            status = Status::Verifying;
        }
        item.spec.http.login = None;
    }
    let error = (status == Status::Error).then(|| {
        let code = report["errorCode"].as_str().unwrap_or_default();
        match code {
            "2" => "Перевищено час очікування".into(),
            "3" | "4" => "Файл не знайдено на сервері".into(),
            "6" => "Помилка мережі".into(),
            "8" => "Сервер не підтримує відновлення".into(),
            "9" => "Недостатньо місця на диску".into(),
            "13" => "Файл уже існує".into(),
            "15" | "16" | "17" | "18" => "Не вдалося записати файл".into(),
            "24" => LOGIN_REJECTED.into(),
            _ if code.is_empty() => "Помилка завантаження".into(),
            _ => format!("Помилка aria2 (код {code})"),
        }
    });
    let changed =
        item.status != status || (!path.is_empty() && item.path != path) || item.error != error;
    item.status = status;
    if !path.is_empty() {
        item.path = path.into();
    }
    item.error = error;
    let total = number(&report["totalLength"]);
    // A restored paused job can report zero lengths until aria2 reads its control file.
    if total > 0 || !matches!(status, Status::Paused | Status::Waiting) {
        item.completed = number(&report["completedLength"]);
        item.total = total;
    }
    item.speed = number(&report["downloadSpeed"]);
    Ok(changed)
}

fn number(value: &Value) -> u64 {
    value
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

pub(crate) fn control_file(path: &Path) -> PathBuf {
    let mut control = path.as_os_str().to_owned();
    control.push(".aria2");
    control.into()
}
