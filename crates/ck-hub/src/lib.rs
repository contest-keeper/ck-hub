//! Keeper Hub: the server the sticks report to. It receives their signed heartbeats and events and
//! shows organizers the state of every stick.

mod accounts;
mod acs_mail;
mod codeforces;
mod download;
mod forge;
mod hub;
mod invite;
mod judge;
mod mail;
mod matcher;
mod postgres_store;
mod register;
mod registry;
mod routes;
mod signup;
mod store;
mod watcher;

pub use accounts::{
    Access, AccessError, Account, CreateUserError, LoginError, MAIL_ALL_WINDOW_SECONDS,
    MAIL_WINDOW_SECONDS, MAX_FAILED_LOGINS, MAX_MAILS_ALL, MAX_MAILS_PER_ADDRESS,
    MIN_PASSWORD_LENGTH, NAME_LOCKOUT_SECONDS, OrganizationError, PasswordCost,
    SESSION_LIFETIME_HOURS, add_member, create_organization, create_user,
};
pub use acs_mail::AcsMailer;
pub use codeforces::{Client as CodeforcesClient, CodeforcesError, parse_status, signed_query};
pub use download::{Download, DownloadError, LINK_LIFETIME_MINUTES};
pub use forge::{FORGE_CODE_LIFETIME_SECONDS, ForgeError};
pub use hub::{Hub, LastHeartbeat, ONLINE_WINDOW_SECONDS, Rejection, StickView};
pub use invite::{AcceptError, INVITATION_LIFETIME_DAYS, InviteError, MemberError, StaffError};
pub use judge::{EventKey, JudgeSubmission, SubmitEvent, submit_event};
pub use mail::{ConsoleMailer, Mail, MailError, Mailer, MemoryMailer, token_after};
pub use matcher::{Verdict, match_submissions};
pub use postgres_store::PostgresStore;
pub use register::{NewStick, RegisterError, Registered, register_sticks};
pub use registry::{Registry, RegistryError};
pub use routes::router;
pub use signup::{
    RESET_LIFETIME_MINUTES, SignupError, TokenError, UNVERIFIED_LIFETIME_HOURS,
    VERIFY_LIFETIME_HOURS,
};
pub use store::{
    Invitation, Member, Membership, MemoryStore, NewUser, Organization, Role, SeenHeartbeat, Stick,
    Store, StoreError, StoredEvent, TokenPurpose, User,
};
pub use watcher::{WatchError, poll_once, run as watch};
