//! The sticks the admin made, read from the registry file that `ck-forge` writes.
//!
//! The Hub only reads this file. It holds every stick's secret, so it is never logged or sent
//! anywhere.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ck_protocol::{StickId, Token};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("cannot read the registry {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the registry {path} is not valid: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

/// One stick, with only the fields the Hub uses. Other fields in the file are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub stick_id: StickId,
    pub token: Token,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub contest: String,
    #[serde(default)]
    pub machine_number: Option<u32>,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    sticks: Vec<Entry>,
}

/// What the file looked like when it was read, to notice when the admin adds a stick.
#[derive(PartialEq, Eq)]
struct Version {
    modified: Option<SystemTime>,
    length: u64,
}

pub struct Registry {
    path: PathBuf,
    version: Version,
    sticks: HashMap<StickId, Entry>,
}

impl Registry {
    /// # Errors
    /// Fails when the file cannot be read or is not a registry.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        let (version, sticks) = read(&path)?;
        Ok(Self {
            path,
            version,
            sticks,
        })
    }

    /// Reads the file again when it changed, so a stick added after the Hub started is known.
    /// A file that cannot be read keeps the sticks already known: a half-written file must not
    /// make every stick unknown.
    pub fn refresh(&mut self) {
        let Ok(current) = version_of(&self.path) else {
            return;
        };
        if current == self.version {
            return;
        }
        match read(&self.path) {
            Ok((version, sticks)) => {
                self.version = version;
                self.sticks = sticks;
            }
            Err(error) => tracing::warn!("keeping the sticks already known: {error}"),
        }
    }

    #[must_use]
    pub fn find(&self, id: &StickId) -> Option<&Entry> {
        self.sticks.get(id)
    }

    /// Every stick, in the order an organizer reads them: by contest, then machine number.
    #[must_use]
    pub fn all(&self) -> Vec<&Entry> {
        let mut sticks: Vec<&Entry> = self.sticks.values().collect();
        sticks.sort_by(|a, b| {
            (&a.contest, a.machine_number, a.stick_id.as_str()).cmp(&(
                &b.contest,
                b.machine_number,
                b.stick_id.as_str(),
            ))
        });
        sticks
    }
}

fn version_of(path: &PathBuf) -> Result<Version, std::io::Error> {
    let meta = fs::metadata(path)?;
    Ok(Version {
        modified: meta.modified().ok(),
        length: meta.len(),
    })
}

fn read(path: &PathBuf) -> Result<(Version, HashMap<StickId, Entry>), RegistryError> {
    let shown = path.display().to_string();
    let version = version_of(path).map_err(|source| RegistryError::Io {
        path: shown.clone(),
        source,
    })?;
    let bytes = fs::read(path).map_err(|source| RegistryError::Io {
        path: shown.clone(),
        source,
    })?;
    let file: File = serde_json::from_slice(&bytes).map_err(|source| RegistryError::Json {
        path: shown,
        source,
    })?;
    let sticks = file
        .sticks
        .into_iter()
        .map(|entry| (entry.stick_id.clone(), entry))
        .collect();
    Ok((version, sticks))
}
