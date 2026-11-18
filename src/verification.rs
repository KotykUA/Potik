use crate::process::{anonymous_file, stop_with_parent};
use gtk::glib;
use ring::digest;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};

const SIGNATURE_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Checks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Arc<[u8]>>,
}

impl Checks {
    pub fn is_empty(&self) -> bool {
        self.sha256.is_none() && self.signature.is_none()
    }

    pub fn run(&self, path: &Path) -> Report {
        let mut messages = Vec::new();
        let mut passed = true;
        if let Some(expected) = &self.sha256 {
            match check_sha256(path, expected) {
                Ok(true) => messages.push("SHA-256 збігається".into()),
                Ok(false) => {
                    messages.push("SHA-256 не збігається".into());
                    passed = false;
                }
                Err(error) => {
                    messages.push(format!("Не вдалося перевірити SHA-256. {error}"));
                    passed = false;
                }
            }
        }
        if let Some(signature) = &self.signature {
            match check_signature(path, signature) {
                Ok(fingerprint) => messages.push(format!("Підпис GPG/PGP дійсний · {fingerprint}")),
                Err(error) => {
                    messages.push(error);
                    passed = false;
                }
            }
        }
        Report {
            passed,
            message: messages.join(" · "),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Report {
    pub passed: bool,
    pub message: String,
}

pub fn sha256_value(value: &str) -> Result<Option<String>, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(Some(value.to_ascii_lowercase()))
    } else {
        Err("SHA-256 має містити рівно 64 шістнадцяткові символи.")
    }
}

pub fn read_signature(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("Виберіть звичайний файл підпису GPG/PGP.".into());
    }
    let mut bytes = Vec::new();
    file.take(SIGNATURE_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() as u64 > SIGNATURE_LIMIT {
        return Err("Файл підпису має бути непорожнім і не більшим за 1 МіБ.".into());
    }
    Ok(bytes)
}

fn check_sha256(path: &Path, expected: &str) -> io::Result<bool> {
    let mut file = File::open(path)?;
    let mut digest = digest::Context::new(&digest::SHA256);
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let actual: String = digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(actual.eq_ignore_ascii_case(expected))
}

fn check_signature(path: &Path, signature: &[u8]) -> Result<String, String> {
    let start_error = |error: io::Error| format!("Не вдалося перевірити підпис GPG/PGP. {error}");
    let mut input = anonymous_file(c"potik-signature", libc::MFD_CLOEXEC).map_err(start_error)?;
    input.write_all(signature).map_err(start_error)?;
    input.rewind().map_err(start_error)?;
    let mut command = Command::new("gpg");
    command.arg("--no-options");
    if std::env::var_os("FLATPAK_ID").is_some() {
        let home = glib::user_data_dir().join("potik/gnupg");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&home)
            .map_err(start_error)?;
        command.arg("--homedir").arg(home);
    }
    command
        .args([
            "--batch",
            "--no-tty",
            "--no-auto-key-retrieve",
            "--no-auto-key-import",
            "--auto-key-locate",
            "local",
            "--status-fd",
            "1",
            "--verify",
            "--",
            "-",
        ])
        .arg(path)
        .stdin(Stdio::from(input))
        .stderr(Stdio::null());
    stop_with_parent(&mut command);
    let output = command.output().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            "Для перевірки підпису GPG/PGP потрібна команда gpg (GnuPG).".into()
        } else {
            start_error(error)
        }
    })?;

    let mut fingerprint = None;
    let mut missing_key = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(status) = line.strip_prefix("[GNUPG:] ") else {
            continue;
        };
        let mut fields = status.split_ascii_whitespace();
        match fields.next() {
            Some("VALIDSIG") => {
                fingerprint = fields
                    .next()
                    .filter(|value| {
                        matches!(value.len(), 40 | 64)
                            && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(str::to_owned)
            }
            Some("NO_PUBKEY") => missing_key = true,
            Some("BADSIG") => return Err("Підпис GPG/PGP не збігається з файлом.".into()),
            Some("EXPSIG" | "EXPKEYSIG" | "REVKEYSIG") => {
                return Err("Строк дії підпису або ключа минув, або ключ відкликано.".into());
            }
            _ => {}
        }
    }
    if missing_key {
        return Err("Відкритий ключ підписанта відсутній у GnuPG.".into());
    }
    fingerprint
        .filter(|_| output.status.success())
        .ok_or_else(|| "Не вдалося підтвердити відокремлений підпис GPG/PGP.".into())
}
