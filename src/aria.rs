use crate::process::{anonymous_file, random_hex, stop_with_parent};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::TcpListener,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Deserialize)]
struct Response {
    id: usize,
    result: Option<Value>,
    error: Option<RpcError>,
}

impl Response {
    fn into_result(self) -> Result<Value> {
        if let Some(error) = self.error {
            return Err(format!("aria2 [{}] — {}", error.code, error.message).into());
        }
        self.result
            .ok_or_else(|| "aria2 повернула порожню відповідь.".into())
    }
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

pub(crate) struct Rpc {
    agent: ureq::Agent,
    url: String,
    token: String,
}

impl Rpc {
    fn new(port: u16, secret: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .build();
        Self {
            agent: config.into(),
            url: format!("http://127.0.0.1:{port}/jsonrpc"),
            token: format!("token:{secret}"),
        }
    }

    fn request<T: DeserializeOwned>(&self, payload: &Value) -> Result<T> {
        let mut response = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            .send(payload.to_string().as_bytes())?;
        Ok(serde_json::from_reader(response.body_mut().as_reader())?)
    }

    fn message(&self, id: usize, method: &str, mut params: Vec<Value>) -> Value {
        params.insert(0, json!(self.token));
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    pub(crate) fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let response: Response = self.request(&self.message(0, method, params))?;
        response.into_result()
    }

    pub(crate) fn batch(&self, calls: Vec<(&str, Vec<Value>)>) -> Result<Vec<Value>> {
        let payload = calls
            .into_iter()
            .enumerate()
            .map(|(id, (method, params))| self.message(id, method, params))
            .collect::<Vec<_>>();
        let mut responses: Vec<Response> = self.request(&json!(payload))?;
        responses.sort_unstable_by_key(|response| response.id);
        if responses.len() != payload.len()
            || responses
                .iter()
                .enumerate()
                .any(|(id, response)| id != response.id)
        {
            return Err("aria2 повернула неповну відповідь.".into());
        }
        responses.into_iter().map(Response::into_result).collect()
    }
}

pub(crate) struct Aria {
    child: Child,
    pub(crate) rpc: Rpc,
    session: RuntimeSession,
    ready: bool,
}

struct RuntimeSession(PathBuf);

impl RuntimeSession {
    fn new() -> io::Result<Self> {
        // aria2 writes its session using a temporary sibling and rename. Keep
        // the raw session in tmpfs because it can contain HTTP credentials.
        let directory = Path::new("/dev/shm").join(format!("potik-{}", random_hex::<12>()?));
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let session = Self(directory);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(session.path())?;
        Ok(session)
    }

    fn path(&self) -> PathBuf {
        self.0.join("queue.session")
    }
}

impl Drop for RuntimeSession {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Aria {
    pub(crate) fn start(session: &Path, connections: u8) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let secret = random_hex::<32>()?;
        let config = rpc_config(&secret)?;
        let runtime_session = RuntimeSession::new()?;
        let mut command = Command::new("aria2c");
        command
            .args([
                "--enable-rpc=true",
                "--rpc-listen-all=false",
                "--rpc-allow-origin-all=false",
                "--follow-torrent=false",
                "--follow-metalink=false",
                "--enable-dht=false",
                "--enable-dht6=false",
                "--bt-enable-lpd=false",
                "--file-allocation=none",
                "--allow-overwrite=false",
                "--auto-file-renaming=true",
                "--max-concurrent-downloads=3",
                "--min-split-size=1M",
                "--stream-piece-selector=default",
                "--enable-http-keep-alive=true",
                "--auto-save-interval=1",
                "--always-resume=true",
                "--retry-wait=3",
                "--max-download-result=1000",
                "--quiet=true",
            ])
            .arg(format!("--split={connections}"))
            .arg(format!("--max-connection-per-server={connections}"))
            .arg(format!("--conf-path=/proc/self/fd/{}", config.as_raw_fd()))
            .arg(format!("--rpc-listen-port={port}"))
            .arg("--input-file")
            .arg(session)
            .arg("--save-session")
            .arg(runtime_session.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        stop_with_parent(&mut command);
        drop(listener);
        let child = command.spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "Команду aria2c не знайдено. Встановіть пакет aria2 і повторіть спробу.".into()
            } else {
                format!("Не вдалося запустити aria2c. {error}")
            }
        })?;
        let mut process = Self {
            child,
            rpc: Rpc::new(port, &secret),
            session: runtime_session,
            ready: false,
        };
        drop(config);

        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Some(status) = process.child.try_wait()? {
                return Err(format!("aria2c завершився під час запуску ({status}).").into());
            }
            if process.rpc.call("aria2.getVersion", vec![]).is_ok() {
                process.ready = true;
                return Ok(process);
            }
            thread::sleep(Duration::from_millis(70));
        }
        Err("aria2c не відповів під час запуску.".into())
    }

    pub(crate) fn session_path(&self) -> PathBuf {
        self.session.path()
    }

    pub(crate) fn stop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if self.ready => {
                let _ = self.rpc.call("aria2.forceShutdown", vec![]);
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            _ => {}
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Aria {
    fn drop(&mut self) {
        self.stop();
    }
}

fn rpc_config(secret: &str) -> io::Result<File> {
    // aria2 requires a regular configuration file. Keep the token in an
    // anonymous file inherited by aria2, outside argv and persistent storage.
    let mut file = anonymous_file(c"potik-aria2", 0)?;
    writeln!(file, "rpc-secret={secret}")?;
    Ok(file)
}
