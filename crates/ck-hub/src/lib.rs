//! Keeper Hub: the server the sticks report to.
//!
//! For now it receives signed heartbeats and lists the sticks with their last heartbeat. The
//! sticks and their secrets come from the registry file `ck-forge` writes.

mod accounts;
mod hub;
mod postgres_store;
mod register;
mod registry;
mod routes;
mod store;

pub use accounts::{
    AccessError, Account, CreateUserError, LoginError, MAX_FAILED_LOGINS, MIN_PASSWORD_LENGTH,
    NAME_LOCKOUT_SECONDS, PasswordCost, SESSION_LIFETIME_HOURS, create_user,
};
pub use hub::{Hub, LastHeartbeat, ONLINE_WINDOW_SECONDS, Rejection, StickView};
pub use postgres_store::PostgresStore;
pub use register::{NewStick, RegisterError, Registered, register_sticks};
pub use registry::{Entry, Registry, RegistryError};
pub use routes::router;
pub use store::{MemoryStore, Role, SeenHeartbeat, Stick, Store, StoreError, StoredEvent, User};
