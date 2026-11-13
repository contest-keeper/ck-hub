//! The sticks the admin made, read from the registry file that `ck-forge` writes.
//!
//! The Hub only reads this file. It holds every stick's secret, so it is never logged or sent
//! anywhere.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use ck_protocol::StickId;
use serde::Deserialize;

use crate::store::Stick;

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

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    sticks: Vec<Stick>,
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
    sticks: HashMap<StickId, Stick>,
}

impl Registry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        let (version, sticks) = read(&path)?;
        Ok(Self {
            path,
            version,
            sticks,
        })
    }

    /// No file and no sticks, for a Hub that gets all its sticks from the database.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            version: Version {
                modified: None,
                length: 0,
            },
            sticks: HashMap::new(),
        }
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
    pub fn find(&self, id: &StickId) -> Option<&Stick> {
        self.sticks.get(id)
    }

    /// Every stick of the file, in no particular order.
    pub fn all(&self) -> impl Iterator<Item = &Stick> {
        self.sticks.values()
    }
}

fn version_of(path: &PathBuf) -> Result<Version, std::io::Error> {
    let meta = fs::metadata(path)?;
    Ok(Version {
        modified: meta.modified().ok(),
        length: meta.len(),
    })
}

fn read(path: &PathBuf) -> Result<(Version, HashMap<StickId, Stick>), RegistryError> {
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
