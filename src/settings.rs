use crate::engine::{default_connections, download_directory, validate_connections};
use gtk::{
    gio::{self, prelude::*},
    glib,
};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
pub struct Settings {
    pub directory: PathBuf,
    pub connections: u8,
    #[serde(default)]
    pub max_speed_kib: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            directory: glib::user_special_dir(glib::UserDirectory::Downloads)
                .unwrap_or_else(|| glib::home_dir().join("Downloads")),
            connections: default_connections(),
            max_speed_kib: 0,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Result<Self, Box<dyn Error>> {
        let data = match fs::read(dir.join("settings.json")) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let settings: Self = serde_json::from_slice(&data)?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self, dir: &Path) -> Result<(), Box<dyn Error>> {
        self.validate()?;
        fs::create_dir_all(dir)?;
        gio::File::for_path(dir.join("settings.json")).replace_contents(
            &serde_json::to_vec_pretty(self)?,
            None,
            false,
            gio::FileCreateFlags::PRIVATE,
            gio::Cancellable::NONE,
        )?;
        Ok(())
    }

    fn validate(&self) -> Result<(), Box<dyn Error>> {
        download_directory(&self.directory)?;
        validate_connections(self.connections)?;
        Ok(())
    }
}
