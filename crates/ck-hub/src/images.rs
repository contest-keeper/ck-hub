//! The Keeper OS images organizers can download. A manifest (`images/images.json`) in the private
//! storage container lists them. The Hub shows organizers the list and sends them to a short-lived
//! link for one image, so the storage address is never published. The files are opaque to the Hub.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::download::Download;

/// Where the manifest lives inside the container. The image pipeline uploads it after the images.
pub const MANIFEST_BLOB: &str = "images/images.json";

/// Every image file lives under this folder of the container.
const BLOB_FOLDER: &str = "images/";

/// A manifest is a few kilobytes. Anything this big is not one.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

const MAX_IMAGES: usize = 200;

/// How long a read manifest is reused, so a page refresh does not call storage every time.
const CACHE_SECONDS: u64 = 60;

/// One image as the manifest describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// Short name used in the download address, for example `keeper-os-0.3.0`.
    pub id: String,
    /// What organizers see.
    pub name: String,
    pub version: String,
    /// The file's path inside the container. It never reaches an organizer.
    pub blob: String,
    /// Compressed size in bytes.
    pub size: u64,
    /// SHA-256 of the file as stored, in lowercase hex, so Forge can check what it downloaded.
    pub sha256: String,
    /// The distribution family, for example `ubuntu`.
    pub family: String,
    /// The hardware profile the image is built for, for example `generic`.
    pub profile: String,
}

#[derive(Deserialize)]
struct Manifest {
    images: Vec<Image>,
}

/// Why a manifest cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ImagesError {
    #[error("the image list cannot be read right now")]
    Unavailable,
    #[error("the image list is not valid: {0}")]
    Invalid(String),
}

/// Where the Hub learns which images exist.
pub trait ImageSource: Send + Sync {
    /// # Errors
    /// [`ImagesError`] when the list cannot be read or is not valid.
    fn list(&self) -> Result<Vec<Image>, ImagesError>;
}

/// A fixed list, for tests and for a developer's machine.
pub struct StaticImages(pub Vec<Image>);

impl ImageSource for StaticImages {
    fn list(&self) -> Result<Vec<Image>, ImagesError> {
        validate(&self.0)?;
        Ok(self.0.clone())
    }
}

/// The manifest kept in the private container, read through a short-lived link.
pub struct StorageImages {
    download: Download,
    agent: ureq::Agent,
    cache: Mutex<Option<(Instant, Vec<Image>)>>,
}

impl StorageImages {
    #[must_use]
    pub fn new(download: Download) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            download,
            agent,
            cache: Mutex::new(None),
        }
    }
}

impl ImageSource for StorageImages {
    fn list(&self) -> Result<Vec<Image>, ImagesError> {
        let mut cache = self.cache.lock().expect("images cache lock");
        if let Some((at, images)) = cache.as_ref()
            && at.elapsed() < Duration::from_secs(CACHE_SECONDS)
        {
            return Ok(images.clone());
        }
        // The link carries a signature, so it is never logged or put in an error.
        let link = self.download.link(MANIFEST_BLOB, Utc::now());
        let response = self
            .agent
            .get(&link)
            .call()
            .map_err(|_| ImagesError::Unavailable)?;
        if !response.status().is_success() {
            return Err(ImagesError::Unavailable);
        }
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ImagesError::Unavailable)?;
        let images = parse(&bytes)?;
        *cache = Some((Instant::now(), images.clone()));
        Ok(images)
    }
}

/// Reads and checks a manifest.
///
/// # Errors
/// [`ImagesError::Invalid`] when it is too big, is not the expected JSON, or an entry is wrong.
pub fn parse(bytes: &[u8]) -> Result<Vec<Image>, ImagesError> {
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ImagesError::Invalid("the manifest is too big".to_owned()));
    }
    let manifest: Manifest =
        serde_json::from_slice(bytes).map_err(|e| ImagesError::Invalid(e.to_string()))?;
    validate(&manifest.images)?;
    Ok(manifest.images)
}

/// One wrong entry rejects the whole list: a half-trusted list is worse than a clear error.
fn validate(images: &[Image]) -> Result<(), ImagesError> {
    if images.len() > MAX_IMAGES {
        return Err(ImagesError::Invalid("too many images".to_owned()));
    }
    for (i, image) in images.iter().enumerate() {
        if let Some(why) = problem(image) {
            return Err(ImagesError::Invalid(format!("image {}: {why}", i + 1)));
        }
        if images[..i].iter().any(|other| other.id == image.id) {
            return Err(ImagesError::Invalid(format!(
                "the id {} is used twice",
                image.id
            )));
        }
    }
    Ok(())
}

fn problem(image: &Image) -> Option<&'static str> {
    let id_ok = (1..=64).contains(&image.id.len())
        && image.id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        });
    if !id_ok {
        return Some(
            "the id must be 1 to 64 lowercase letters, digits, dashes, dots or underscores",
        );
    }
    for (text, name) in [
        (&image.name, "name"),
        (&image.version, "version"),
        (&image.family, "family"),
        (&image.profile, "profile"),
    ] {
        if text.is_empty() || text.len() > 100 || text.chars().any(char::is_control) {
            return Some(match name {
                "name" => "the name must be 1 to 100 characters without control characters",
                "version" => "the version must be 1 to 100 characters without control characters",
                "family" => "the family must be 1 to 100 characters without control characters",
                _ => "the profile must be 1 to 100 characters without control characters",
            });
        }
    }
    if !is_safe_blob(&image.blob) {
        return Some("the file path must be inside images/ with plain names only");
    }
    if image.size == 0 {
        return Some("the size must be more than zero");
    }
    let hash_ok = image.sha256.len() == 64
        && image
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hash_ok {
        return Some("the sha256 must be 64 lowercase hex digits");
    }
    None
}

/// A path under `images/` made of plain names, so it cannot climb out of the folder or add a query.
fn is_safe_blob(blob: &str) -> bool {
    let Some(rest) = blob.strip_prefix(BLOB_FOLDER) else {
        return false;
    };
    !rest.is_empty()
        && blob.len() <= 200
        && rest.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str) -> Image {
        Image {
            id: id.to_owned(),
            name: "Keeper OS".to_owned(),
            version: "0.3.0".to_owned(),
            blob: format!("images/{id}.img.zst"),
            size: 1_500_000_000,
            sha256: "a".repeat(64),
            family: "ubuntu".to_owned(),
            profile: "generic".to_owned(),
        }
    }

    #[test]
    fn a_good_manifest_is_read() {
        let text = serde_json::json!({ "images": [image("keeper-os-0.3.0")] }).to_string();
        let images = parse(text.as_bytes()).expect("valid");
        assert_eq!(images, [image("keeper-os-0.3.0")]);
    }

    #[test]
    fn a_path_that_leaves_the_folder_is_refused() {
        for blob in [
            "other/x.img",
            "images/../secret",
            "images//x",
            "images/x?sv=1",
            "images/",
            "images/a b",
            "images\\x",
        ] {
            let mut bad = image("x");
            bad.blob = blob.to_owned();
            assert!(validate(&[bad]).is_err(), "{blob}");
        }
    }

    #[test]
    fn bad_ids_hashes_sizes_and_repeats_are_refused() {
        assert!(validate(&[image("Upper")]).is_err());
        assert!(validate(&[image("")]).is_err());
        let mut short_hash = image("x");
        short_hash.sha256 = "abc".to_owned();
        assert!(validate(&[short_hash]).is_err());
        let mut upper_hash = image("x");
        upper_hash.sha256 = "A".repeat(64);
        assert!(validate(&[upper_hash]).is_err());
        let mut empty = image("x");
        empty.size = 0;
        assert!(validate(&[empty]).is_err());
        assert!(validate(&[image("x"), image("x")]).is_err());
    }

    #[test]
    fn a_manifest_that_is_not_json_or_too_big_is_refused() {
        assert!(parse(b"nope").is_err());
        assert!(parse(&vec![b' '; 1024 * 1024 + 1]).is_err());
    }
}
