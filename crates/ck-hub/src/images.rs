//! The Keeper OS images organizers can download. A manifest (`images/images.json`) in the private
//! storage container lists them. The Hub shows organizers the list and sends them to a short-lived
//! link for one image, so the storage address is never published. The files are opaque to the Hub.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

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
    /// Public name of the downloaded file. Older manifests omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
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
    /// The written offer of the source code of the image's free software (GPL and similar), kept
    /// next to the image. Its path inside the container never reaches an organizer; it is served at
    /// `/download/image/<id>/source-offer`. Images made before the offer existed have none.
    #[serde(default)]
    pub source_offer: Option<String>,
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
        let link = self
            .download
            .link(MANIFEST_BLOB, OffsetDateTime::now_utc())
            .map_err(|_| ImagesError::Unavailable)?;
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
    if image
        .filename
        .as_deref()
        .is_some_and(|filename| !is_safe_filename(filename))
    {
        return Some("the filename must be a plain basename of at most 200 bytes");
    }
    if image
        .source_offer
        .as_deref()
        .is_some_and(|b| !is_safe_blob(b))
    {
        return Some("the source offer path must be inside images/ with plain names only");
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

fn is_safe_filename(filename: &str) -> bool {
    let bytes = filename.as_bytes();
    bytes.len() <= 200
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && filename
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
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

/// The ids of the newest image of each family and profile: what the downloads page shows first.
///
/// The manifest stores no publish time, so "newest" is derived from the version. A normal release
/// (`1.2.3`) always beats a pre-release (`1.2.3-dev.7`), even a pre-release of a higher number; a
/// group with no release falls back to its newest pre-release. Within the same kind, the higher
/// version wins (numbers compared as numbers, pre-release parts as in semver). A version that is
/// not `1.2.3` or `1.2.3-suffix` ranks last, and a tie goes to the entry later in the manifest,
/// because the publish tool appends.
#[must_use]
pub fn latest_ids(images: &[Image]) -> std::collections::HashSet<String> {
    let mut best: std::collections::HashMap<(&str, &str), (VersionRank, &str)> =
        std::collections::HashMap::new();
    for image in images {
        let rank = VersionRank::of(&image.version);
        let key = (image.family.as_str(), image.profile.as_str());
        match best.get(&key) {
            Some((current, _)) if rank < *current => {}
            _ => {
                best.insert(key, (rank, image.id.as_str()));
            }
        }
    }
    best.into_values().map(|(_, id)| id.to_owned()).collect()
}

#[derive(Debug, PartialEq, Eq)]
struct VersionRank {
    valid: bool,
    release: bool,
    core: Vec<u64>,
    pre: Vec<PreIdentifier>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PreIdentifier {
    Number(String),
    Text(String),
}

impl VersionRank {
    fn of(version: &str) -> Self {
        let (core, pre) = version
            .split_once('-')
            .map_or((version, ""), |(core, pre)| (core, pre));
        let numbers: Option<Vec<u64>> = core.split('.').map(|part| part.parse().ok()).collect();
        let Some(core) = numbers else {
            return Self {
                valid: false,
                release: false,
                core: Vec::new(),
                pre: Vec::new(),
            };
        };
        Self {
            valid: true,
            release: !version.contains('-'),
            core,
            pre: pre
                .split('.')
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if part.bytes().all(|b| b.is_ascii_digit()) {
                        // Digit strings order like numbers by length, then text, once zeros are gone.
                        PreIdentifier::Number(part.trim_start_matches('0').to_owned())
                    } else {
                        PreIdentifier::Text(part.to_owned())
                    }
                })
                .collect(),
        }
    }
}

impl Ord for VersionRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let numbers = |a: &str, b: &str| a.len().cmp(&b.len()).then_with(|| a.cmp(b));
        let pre = || {
            self.pre
                .iter()
                .zip(&other.pre)
                .map(|pair| match pair {
                    (PreIdentifier::Number(a), PreIdentifier::Number(b)) => numbers(a, b),
                    (a, b) => a.cmp(b),
                })
                .find(|order| order.is_ne())
                .unwrap_or_else(|| self.pre.len().cmp(&other.pre.len()))
        };
        self.valid
            .cmp(&other.valid)
            .then_with(|| self.release.cmp(&other.release))
            .then_with(|| self.core.cmp(&other.core))
            .then_with(pre)
    }
}

impl PartialOrd for VersionRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: &str) -> Image {
        Image {
            id: id.to_owned(),
            name: "Keeper OS".to_owned(),
            version: "0.3.0".to_owned(),
            filename: Some(format!("{id}.img.zst")),
            blob: format!("images/{id}.img.zst"),
            size: 1_500_000_000,
            sha256: "a".repeat(64),
            family: "ubuntu".to_owned(),
            profile: "generic".to_owned(),
            source_offer: Some(format!("images/{id}.SOURCE-OFFER.txt")),
        }
    }

    #[test]
    fn a_good_manifest_is_read() {
        let text = serde_json::json!({ "images": [image("keeper-os-0.3.0")] }).to_string();
        let images = parse(text.as_bytes()).expect("valid");
        assert_eq!(images, [image("keeper-os-0.3.0")]);
    }

    #[test]
    fn a_public_filename_must_be_one_plain_basename() {
        let long = "x".repeat(201);
        for filename in [
            "",
            "../keeper.img.zst",
            "images/keeper.img.zst",
            "images\\keeper.img.zst",
            ".hidden.img.zst",
            "keeper.img.zst.",
            "keeper.img.zst?download",
            "keeper image.img.zst",
            "kéeper.img.zst",
            &long,
        ] {
            let mut entry = serde_json::to_value(image("x")).expect("entry");
            entry["filename"] = serde_json::json!(filename);
            let manifest = serde_json::json!({ "images": [entry] }).to_string();
            assert!(parse(manifest.as_bytes()).is_err(), "{filename:?}");
        }
    }

    #[test]
    fn a_source_offer_outside_the_folder_is_refused_and_a_missing_one_is_fine() {
        let mut bad = image("x");
        bad.source_offer = Some("images/../secret".to_owned());
        let text = serde_json::json!({ "images": [bad] }).to_string();
        assert!(parse(text.as_bytes()).is_err());
        let old = r#"{"images":[{"id":"x","name":"n","version":"1","blob":"images/x.img",
            "size":1,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "family":"ubuntu","profile":"generic"}]}"#;
        let old_images = parse(old.as_bytes()).expect("valid legacy manifest");
        assert_eq!(old_images[0].source_offer, None);
        assert_eq!(old_images[0].filename, None);
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

    fn versioned(id: &str, version: &str, family: &str) -> Image {
        let mut entry = image(id);
        entry.version = version.to_owned();
        entry.family = family.to_owned();
        entry
    }

    fn latest(images: &[Image]) -> Vec<String> {
        let mut ids: Vec<_> = latest_ids(images).into_iter().collect();
        ids.sort();
        ids
    }

    #[test]
    fn a_release_beats_a_newer_looking_pre_release() {
        let images = [
            versioned("a", "0.1.0", "ubuntu"),
            versioned("b", "0.1.0-dev-unlock-20261119", "ubuntu"),
            versioned("c", "0.2.0-dev.1.1", "ubuntu"),
            versioned("d", "0.0.9", "ubuntu"),
        ];
        assert_eq!(latest(&images), ["a"]);
    }

    #[test]
    fn releases_compare_as_numbers() {
        let images = [
            versioned("a", "0.9.0", "ubuntu"),
            versioned("b", "0.10.0", "ubuntu"),
            versioned("c", "0.2.5", "ubuntu"),
        ];
        assert_eq!(latest(&images), ["b"]);
    }

    #[test]
    fn without_a_release_the_newest_pre_release_wins() {
        let images = [
            versioned("a", "0.1.0-dev.44633031834.2", "ubuntu"),
            versioned("b", "0.1.0-dev.44701771938.1", "ubuntu"),
            versioned("c", "0.1.0-dev.9.9", "ubuntu"),
        ];
        assert_eq!(latest(&images), ["b"]);
    }

    #[test]
    fn each_family_and_profile_gets_its_own_latest() {
        let mut other_profile = versioned("e", "0.1.0", "ubuntu");
        other_profile.profile = "rpi".to_owned();
        let images = [
            versioned("a", "0.1.0", "ubuntu"),
            versioned("b", "0.2.0", "ubuntu"),
            versioned("c", "0.1.0", "mint"),
            versioned("d", "0.1.0-dev.1", "fedora"),
            other_profile,
        ];
        assert_eq!(latest(&images), ["b", "c", "d", "e"]);
    }

    #[test]
    fn an_odd_version_ranks_last_and_a_tie_goes_to_the_later_entry() {
        let images = [
            versioned("a", "nightly", "ubuntu"),
            versioned("b", "0.0.1-dev", "ubuntu"),
        ];
        assert_eq!(latest(&images), ["b"]);
        let images = [
            versioned("a", "nightly", "ubuntu"),
            versioned("b", "latest", "ubuntu"),
        ];
        assert_eq!(latest(&images), ["b"]);
        assert!(latest_ids(&[]).is_empty());
    }
}
