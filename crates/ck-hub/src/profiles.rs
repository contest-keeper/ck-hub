//! Contest profiles of Keeper Forge, kept for the organization so a team shares them.
//!
//! A profile is the forge's settings JSON, which the Hub keeps as it is once it is valid JSON of a
//! sane size, and an optional wallpaper picture. The picture's bytes are kept in the database row
//! of the profile.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use crate::hub::Hub;
use crate::store::{NewProfile, Profile, ProfileSummary, StoreError, Wallpaper};

/// The settings of a contest are a few kilobytes; this leaves room and stops a mistake.
pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;
/// The picture after it is decoded.
pub const MAX_WALLPAPER_BYTES: usize = 5 * 1024 * 1024;
const MAX_ID_LENGTH: usize = 64;
const MAX_NAME_LENGTH: usize = 200;
const MAX_WALLPAPER_NAME_LENGTH: usize = 255;

#[derive(Debug, Deserialize)]
pub struct ProfileInput {
    pub name: String,
    pub settings: serde_json::Value,
    pub wallpaper: Option<WallpaperInput>,
}

#[derive(Debug, Deserialize)]
pub struct WallpaperInput {
    pub name: String,
    pub data_base64: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProfileError {
    /// Something in the request is wrong. Nothing was kept.
    Invalid(String),
    /// The settings or the picture are over the limit.
    TooLarge(String),
    Unavailable,
}

/// The content type of a picture name the Hub accepts, from its extension.
pub(crate) fn content_type(name: &str) -> Option<&'static str> {
    let (_, extension) = name.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

fn check_id(id: &str) -> Result<(), ProfileError> {
    if id.is_empty()
        || id.len() > MAX_ID_LENGTH
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ProfileError::Invalid(format!(
            "a profile id is 1 to {MAX_ID_LENGTH} letters, digits, - or _"
        )));
    }
    Ok(())
}

fn check_wallpaper_name(name: &str) -> Result<&'static str, ProfileError> {
    let invalid = |why: &str| Err(ProfileError::Invalid(format!("the wallpaper name {why}")));
    if name.is_empty() || name.len() > MAX_WALLPAPER_NAME_LENGTH {
        return invalid("is empty or too long");
    }
    if name == "."
        || name == ".."
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return invalid("must be a plain file name");
    }
    match content_type(name) {
        Some(kind) => Ok(kind),
        None => invalid("must end in .png, .jpg or .jpeg"),
    }
}

fn looks_like(kind: &str, data: &[u8]) -> bool {
    match kind {
        "image/png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
        _ => data.starts_with(&[0xff, 0xd8, 0xff]),
    }
}

fn unavailable(error: &StoreError) -> ProfileError {
    tracing::error!("{error}");
    ProfileError::Unavailable
}

impl Hub {
    pub fn put_profile(
        &self,
        organization: &str,
        id: &str,
        input: ProfileInput,
    ) -> Result<(), ProfileError> {
        check_id(id)?;
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_LENGTH {
            return Err(ProfileError::Invalid(format!(
                "a profile name is 1 to {MAX_NAME_LENGTH} characters"
            )));
        }
        let size = serde_json::to_vec(&input.settings).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_SETTINGS_BYTES {
            return Err(ProfileError::TooLarge(format!(
                "the settings are over {} KiB",
                MAX_SETTINGS_BYTES / 1024
            )));
        }
        let wallpaper = match input.wallpaper {
            None => None,
            Some(picture) => {
                let kind = check_wallpaper_name(&picture.name)?;
                // Four characters carry three bytes, so this stops a huge string before decoding.
                if picture.data_base64.len() > MAX_WALLPAPER_BYTES.div_ceil(3) * 4 {
                    return Err(ProfileError::TooLarge(wallpaper_too_large()));
                }
                let data = STANDARD
                    .decode(picture.data_base64.as_bytes())
                    .map_err(|_| ProfileError::Invalid("the wallpaper is not base64".to_owned()))?;
                if data.len() > MAX_WALLPAPER_BYTES {
                    return Err(ProfileError::TooLarge(wallpaper_too_large()));
                }
                if !looks_like(kind, &data) {
                    return Err(ProfileError::Invalid(
                        "the wallpaper is not the kind of picture its name says".to_owned(),
                    ));
                }
                Some(Wallpaper {
                    name: picture.name,
                    data,
                })
            }
        };
        let profile = NewProfile {
            id: id.to_owned(),
            name: name.to_owned(),
            settings: input.settings,
            wallpaper,
        };
        self.store
            .put_profile(organization, profile, (self.clock)())
            .map_err(|e| unavailable(&e))
    }

    pub fn profiles(&self, organization: &str) -> Result<Vec<ProfileSummary>, StoreError> {
        self.store.profiles(organization)
    }

    pub fn profile(&self, organization: &str, id: &str) -> Result<Option<Profile>, StoreError> {
        self.store.profile(organization, id)
    }

    pub fn profile_wallpaper(
        &self,
        organization: &str,
        id: &str,
    ) -> Result<Option<Wallpaper>, StoreError> {
        self.store.profile_wallpaper(organization, id)
    }

    pub fn delete_profile(&self, organization: &str, id: &str) -> Result<bool, StoreError> {
        self.store.delete_profile(organization, id)
    }
}

fn wallpaper_too_large() -> String {
    format!(
        "the wallpaper is over {} MB",
        MAX_WALLPAPER_BYTES / (1024 * 1024)
    )
}
