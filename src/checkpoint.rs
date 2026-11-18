use crate::engine::{Download, control_file};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    error::Error,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Deserialize, Serialize)]
struct SavedProgress {
    path: String,
    inode: u64,
    length: u64,
    control: Vec<u8>,
}

pub struct Checkpoints {
    directory: PathBuf,
    saved: HashMap<String, (u64, SystemTime)>,
}

impl Checkpoints {
    pub fn new(state: &Path) -> io::Result<Self> {
        let directory = state.join("checkpoints");
        fs::create_dir_all(&directory)?;
        File::open(state)?.sync_all()?;
        Ok(Self {
            directory,
            saved: HashMap::new(),
        })
    }

    pub fn save(&mut self, items: &[Download]) {
        for item in items
            .iter()
            .filter(|item| item.status.is_live() && item.completed > 0 && !item.path.is_empty())
        {
            if let Err(error) = self.save_item(item) {
                eprintln!("potik: checkpoint {}: {error}", item.gid);
            }
        }
    }

    fn save_item(&mut self, item: &Download) -> Result<()> {
        let path = Path::new(&item.path);
        let Some(mut control) = open_if_exists(&control_file(path))? else {
            return Ok(());
        };
        let control_metadata = control.metadata()?;
        let version = (control_metadata.ino(), control_metadata.modified()?);
        if self.saved.get(&item.gid) == Some(&version) {
            return Ok(());
        }
        let mut contents = Vec::new();
        control.read_to_end(&mut contents)?;
        if contents.is_empty() {
            return Ok(());
        }
        // aria2 publishes its piece map after flushing its write cache. Make
        // those bytes durable before publishing our copy of that exact map.
        let Some(metadata) = sync_download(path)? else {
            return Ok(());
        };
        let progress = SavedProgress {
            path: item.path.clone(),
            inode: metadata.ino(),
            length: metadata.len(),
            control: contents,
        };
        write_atomic(&self.path(&item.gid), |file| {
            serde_json::to_writer(file, &progress)?;
            Ok(())
        })?;
        self.saved.insert(item.gid.clone(), version);
        Ok(())
    }

    pub fn restore(&self, items: &[Download]) {
        for item in items
            .iter()
            .filter(|item| item.status.is_live() && !item.path.is_empty())
        {
            if let Err(error) = self.restore_item(item) {
                eprintln!("potik: restore checkpoint {}: {error}", item.gid);
            }
        }
    }

    fn restore_item(&self, item: &Download) -> Result<()> {
        let Some(saved) = open_if_exists(&self.path(&item.gid))? else {
            return Ok(());
        };
        let progress: SavedProgress = serde_json::from_reader(saved)?;
        let path = Path::new(&item.path);
        let Some(data) = open_if_exists(path)? else {
            return Ok(());
        };
        let metadata = data.metadata()?;
        if progress.path == item.path
            && metadata.ino() == progress.inode
            && metadata.len() >= progress.length
        {
            write_atomic(&control_file(path), |file| {
                file.write_all(&progress.control)?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub fn discard(&mut self, gid: &str) {
        self.saved.remove(gid);
        if let Err(error) = fs::remove_file(self.path(gid))
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("potik: remove checkpoint {gid}: {error}");
        }
    }

    fn path(&self, gid: &str) -> PathBuf {
        self.directory.join(format!("{gid}.json"))
    }
}

pub fn sync_download(path: &Path) -> Result<Option<Metadata>> {
    let Some(data) = open_if_exists(path)? else {
        return Ok(None);
    };
    let metadata = data.metadata()?;
    data.sync_all()?;
    let parent = path
        .parent()
        .ok_or("Не вдалося визначити папку завантаження.")?;
    // Some download locations are exposed through filesystems that implement
    // file fsync but do not offer directory fsync.
    if let Err(error) = File::open(parent)?.sync_all()
        && !matches!(
            error.raw_os_error(),
            Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
        )
    {
        return Err(error.into());
    }
    Ok(Some(metadata))
}

fn open_if_exists(path: &Path) -> io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) if file.metadata()?.is_file() => Ok(Some(file)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_atomic(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or("Не вдалося визначити папку для збереження.")?;
    let tmp = parent.join(format!(
        ".potik-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
