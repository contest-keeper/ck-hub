//! Keeper Hub: the server the sticks report to. It receives their signed heartbeats and events and
//! shows organizers the state of every stick.

mod accounts;
mod acs_mail;
mod codeforces;
mod commands;
mod contest;
mod credentials;
mod csrf;
mod delegation;
mod download;
mod enrol;
mod forge;
mod hub;
mod images;
mod invite;
mod judge;
mod mail;
mod matcher;
mod network;
mod postgres_store;
mod profiles;
mod recordings;
mod register;
mod registry;
mod routes;
mod signing;
mod signup;
mod source;
mod store;
mod submissions;
mod watcher;

pub use accounts::{
    Access, AccessError, Account, CreateUserError, LoginError, MAIL_ALL_WINDOW_SECONDS,
    MAIL_WINDOW_SECONDS, MAX_FAILED_LOGINS, MAX_MAILS_ALL, MAX_MAILS_PER_ADDRESS,
    MIN_PASSWORD_LENGTH, NAME_LOCKOUT_SECONDS, OrganizationError, PasswordCost,
    SESSION_LIFETIME_HOURS, add_member, create_organization, create_user,
};
pub use acs_mail::AcsMailer;
pub use codeforces::{
    Client as CodeforcesClient, CodeforcesError, Judge, parse_status, signed_query,
};
pub use commands::{
    CommandError, CommandState, CommandView, DEFAULT_LIFETIME_SECONDS, HISTORY_DAYS, HISTORY_LIMIT,
    MAX_OPEN_PER_STICK, MAX_PER_MINUTE, MIN_LIFETIME_SECONDS, StoredCommand,
};
pub use contest::{ContestRow, ContestView, MatchedBy};
pub use credentials::CredentialScope;
pub use delegation::Delegation;
pub use download::{Download, DownloadError, LINK_LIFETIME_MINUTES};
pub use enrol::{
    DEFAULT_LIFETIME_MINUTES, EnrolError, EnrolledStick, Enrolment, EnrolmentSpec,
    MAX_FAILED_PER_MINUTE, MAX_LIFETIME_MINUTES, MAX_MINTS_PER_MINUTE, MIN_LIFETIME_MINUTES,
    MintedCode, NewEnrolmentCode, Redemption,
};
pub use forge::{FORGE_CODE_LIFETIME_SECONDS, ForgeError};
pub use hub::{HardwareView, Hub, LastHeartbeat, ONLINE_WINDOW_SECONDS, Rejection, StickView};
pub use images::{Image, ImageSource, ImagesError, MANIFEST_BLOB, StaticImages, StorageImages};
pub use invite::{AcceptError, INVITATION_LIFETIME_DAYS, InviteError, MemberError, StaffError};
pub use judge::{EventKey, JudgeSubmission, SubmitEvent, submit_event};
pub use mail::{ConsoleMailer, Mail, MailError, Mailer, MemoryMailer, token_after};
pub use matcher::{Verdict, match_submissions};
pub use postgres_store::{DEFAULT_POOL_SIZE, PostgresStore};
pub use profiles::{
    MAX_SETTINGS_BYTES, MAX_WALLPAPER_BYTES, ProfileError, ProfileInput, WallpaperInput,
};
pub use recordings::{
    ABANDONED_DAYS, AzureBlobs, BlobError, Blobs, CHUNK_BYTES, DEFAULT_RETENTION_DAYS, Kind,
    MAX_CHUNKS_PER_MINUTE, MAX_FILE_BYTES, MAX_RETENTION_DAYS, MAX_STICK_BYTES, MemoryBlobs,
    Recording, RecordingError, RecordingView, UploadStatus, VIEW_LINK_HOURS, block_id,
};
pub use register::{NewStick, RegisterError, Registered, register_sticks};
pub use registry::{Registry, RegistryError};
pub use routes::router;
pub use signing::{MAX_SIGNATURES_PER_MINUTE, MAX_SIGNED_SETTINGS_BYTES, SignError};
pub use signup::{
    RESET_LIFETIME_MINUTES, SignupError, TokenError, UNVERIFIED_LIFETIME_HOURS,
    VERIFY_LIFETIME_HOURS,
};
pub use store::{
    BootSpan, Hardware, Invitation, Member, Membership, MemoryStore, NewProfile, NewUser,
    Organization, Profile, ProfileSummary, Removal, Role, SeenHeartbeat, Stick, StickHardware,
    Store, StoreError, StoredEvent, TokenPurpose, User, Wallpaper,
};
pub use submissions::{JudgeRow, StickSubmission, SubmissionsView};
pub use watcher::{WatchError, poll_once, run as watch};
