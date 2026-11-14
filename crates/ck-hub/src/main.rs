//! `ck-hub`: runs the Hub server, and makes organizer accounts for it.

use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use ck_hub::{
    CreateUserError, Download, Hub, MIN_PASSWORD_LENGTH, MemoryStore, NewStick, OrganizationError,
    PasswordCost, PostgresStore, RegisterError, Registry, Role, Store, add_member,
    create_organization, create_user, register_sticks, router,
};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "ck-hub",
    version,
    about = "The Contest Keeper server",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Make an account. The password is asked for here and never taken from an argument
    CreateUser(CreateUserArgs),
    /// Make an organization and its first owner, who must have an account
    CreateOrg(CreateOrgArgs),
    /// Make an existing account a member of an organization
    AddMember(AddMemberArgs),
    /// Register the sticks of an admin tool registry file in the database. Safe to run twice
    ImportRegistry(ImportRegistryArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Where to listen. Sticks in a QEMU test machine reach the host as 10.0.2.2
    #[arg(long, env = "CK_LISTEN", default_value = "127.0.0.1:8088")]
    listen: SocketAddr,
    /// A registry file ck-forge writes, read directly. It holds every stick's secret, so keep it
    /// private. Optional when sticks are registered in the database instead
    #[arg(long)]
    registry: Option<PathBuf>,
    #[command(flatten)]
    database: DatabaseArgs,
    /// A file to add one line to for every heartbeat, accepted or not. No secrets go in it
    #[arg(long)]
    log: Option<PathBuf>,
    #[command(flatten)]
    download: DownloadArgs,
}

// The field names make the flags `--download-account` and so on, so the shared prefix stays.
#[allow(clippy::struct_field_names)]
#[derive(Args)]
struct DownloadArgs {
    /// The Azure Storage account that holds the downloads. Without it `/download/...` answers 404
    #[arg(long, env = "CK_DOWNLOAD_ACCOUNT", requires = "download_key")]
    download_account: Option<String>,
    /// The private container inside that account
    #[arg(long, env = "CK_DOWNLOAD_CONTAINER", default_value = "downloads")]
    download_container: String,
    /// The account key the short-lived download links are signed with
    #[arg(
        long,
        env = "CK_DOWNLOAD_KEY",
        hide_env_values = true,
        requires = "download_account"
    )]
    download_key: Option<String>,
}

#[derive(Args)]
struct DatabaseArgs {
    /// The PostgreSQL address, like `postgres://user:password@localhost/ckhub`. Without it the Hub
    /// keeps everything in memory and forgets it when it stops
    #[arg(long, env = "CK_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
}

#[derive(Args)]
struct CreateUserArgs {
    /// The email the person logs in with
    email: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[derive(Args)]
struct CreateOrgArgs {
    /// The short name used in addresses: lowercase letters, digits and hyphens
    slug: String,
    /// The name people see. Defaults to the slug
    #[arg(long)]
    name: Option<String>,
    /// The email of the account that becomes the owner
    #[arg(long)]
    owner: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[derive(Args)]
struct AddMemberArgs {
    /// The slug of the organization
    slug: String,
    /// The email of the account to add
    email: String,
    /// `owner`, `admin` or `viewer`
    #[arg(long, default_value = "viewer")]
    role: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[derive(Args)]
struct ImportRegistryArgs {
    /// The registry file ck-forge wrote
    file: PathBuf,
    /// The slug of the organization the sticks go to
    #[arg(long)]
    org: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_ansi(false).init();
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::CreateUser(args)) => make_user(args).await,
        Some(Command::CreateOrg(args)) => make_org(args).await,
        Some(Command::AddMember(args)) => add_to_org(args).await,
        Some(Command::ImportRegistry(args)) => import_registry(args).await,
        None => run(cli.serve).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Connects to the database. The client blocks and starts a runtime of its own, so it must not
/// connect on a thread that already runs async tasks.
async fn connect(url: String) -> Result<PostgresStore, String> {
    tokio::task::spawn_blocking(move || PostgresStore::connect(&url))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

async fn make_user(args: CreateUserArgs) -> Result<(), String> {
    let url = args
        .database
        .database_url
        .ok_or("accounts need a database: pass --database-url or set CK_DATABASE_URL")?;

    // Typed at a prompt without echo, twice. It is never read from an argument or the environment,
    // so it does not end up in a shell history or a process list.
    let password = rpassword::prompt_password(format!(
        "Password for {} (at least {MIN_PASSWORD_LENGTH} characters): ",
        args.email
    ))
    .map_err(|e| e.to_string())?;
    let again = rpassword::prompt_password("Password again: ").map_err(|e| e.to_string())?;
    if password != again {
        return Err("the two passwords are not the same".to_owned());
    }

    let store = connect(url).await?;
    let email = args.email.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = create_user(
            &store,
            PasswordCost::default(),
            Utc::now(),
            &email,
            &password,
        );
        // The store is dropped on this blocking thread too, where closing the connection is safe.
        drop(store);
        result
    })
    .await
    .map_err(|e| e.to_string())?;
    match result {
        Ok(_) => {
            // Said as a log line, like everything else this program says.
            tracing::info!(email = %args.email, "the account was made");
            Ok(())
        }
        Err(CreateUserError::BadEmail) => Err("that is not an email address".to_owned()),
        Err(CreateUserError::PasswordTooShort) => Err(format!(
            "the password must have at least {MIN_PASSWORD_LENGTH} characters"
        )),
        Err(CreateUserError::EmailTaken) => {
            Err("an account with that email already exists".to_owned())
        }
        Err(CreateUserError::Unavailable) => Err("the database could not be written".to_owned()),
    }
}

/// Runs `work` against the database on a thread where blocking is fine.
async fn with_store<T: Send + 'static>(
    database: DatabaseArgs,
    work: impl FnOnce(&PostgresStore) -> T + Send + 'static,
) -> Result<T, String> {
    let url = database
        .database_url
        .ok_or("this needs a database: pass --database-url or set CK_DATABASE_URL")?;
    let store = connect(url).await?;
    tokio::task::spawn_blocking(move || {
        let result = work(&store);
        // Dropped on this blocking thread, where closing the connection is safe.
        drop(store);
        result
    })
    .await
    .map_err(|e| e.to_string())
}

fn organization_problem(error: &OrganizationError) -> String {
    match error {
        OrganizationError::BadSlug => "the slug must be lowercase letters, digits and hyphens \
             (not at the ends), at most 63 characters, and the name must not be empty"
            .to_owned(),
        OrganizationError::SlugTaken => "an organization with that slug exists".to_owned(),
        OrganizationError::NoSuchOrganization => {
            "there is no organization with that slug".to_owned()
        }
        OrganizationError::NoSuchUser => "there is no account with that email".to_owned(),
        OrganizationError::AlreadyMember => "that account is already a member".to_owned(),
        OrganizationError::Unavailable => "the database could not be written".to_owned(),
    }
}

async fn make_org(args: CreateOrgArgs) -> Result<(), String> {
    let name = args.name.unwrap_or_else(|| args.slug.clone());
    let (slug, owner) = (args.slug, args.owner);
    let made = {
        let (slug, owner) = (slug.clone(), owner.clone());
        with_store(args.database, move |store| {
            create_organization(store, &slug, &name, &owner)
        })
        .await?
    };
    match made {
        Ok(_) => {
            tracing::info!(slug = %slug, owner = %owner, "the organization was made");
            Ok(())
        }
        Err(error) => Err(organization_problem(&error)),
    }
}

async fn add_to_org(args: AddMemberArgs) -> Result<(), String> {
    let role = Role::parse(&args.role).ok_or("the role must be owner, admin or viewer")?;
    let (slug, email) = (args.slug, args.email);
    let added = {
        let (slug, email) = (slug.clone(), email.clone());
        with_store(args.database, move |store| {
            add_member(store, &slug, &email, role)
        })
        .await?
    };
    match added {
        Ok(()) => {
            tracing::info!(slug = %slug, email = %email, role = %args.role, "the member was added");
            Ok(())
        }
        Err(error) => Err(organization_problem(&error)),
    }
}

/// The part of a registry file the Hub needs.
#[derive(serde::Deserialize)]
struct File {
    sticks: Vec<NewStick>,
}

async fn import_registry(args: ImportRegistryArgs) -> Result<(), String> {
    let url = args
        .database
        .database_url
        .ok_or("importing needs a database: pass --database-url or set CK_DATABASE_URL")?;
    let text = std::fs::read_to_string(&args.file)
        .map_err(|e| format!("cannot read {}: {e}", args.file.display()))?;
    let file: File = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not a registry file: {e}", args.file.display()))?;

    let store = connect(url).await?;
    let slug = args.org;
    let result = tokio::task::spawn_blocking(move || {
        let result = match store.organization_by_slug(&slug) {
            Ok(Some(organization)) => Ok(register_sticks(
                &store,
                &organization.id,
                &|_| None,
                file.sticks,
            )),
            Ok(None) => Err("there is no organization with that slug".to_owned()),
            Err(error) => Err(error.to_string()),
        };
        drop(store);
        result
    })
    .await
    .map_err(|e| e.to_string())??;
    match result {
        Ok(done) => {
            tracing::info!(
                added = done.added,
                already_known = done.already_known,
                "the registry was imported"
            );
            if done.conflicts.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "these sticks are already registered with a different secret and were left as they are: {}",
                    done.conflicts.join(", ")
                ))
            }
        }
        Err(RegisterError::Invalid(why)) => Err(why),
        Err(RegisterError::Unavailable) => Err("the database could not be written".to_owned()),
    }
}

async fn run(args: ServeArgs) -> Result<(), String> {
    // Sticks come from the database, from this file, or from both. A Hub that has neither knows no
    // stick, and every heartbeat is refused.
    let registry = match &args.registry {
        Some(path) => Registry::open(path).map_err(|e| e.to_string())?,
        None => Registry::empty(),
    };
    let log = match &args.log {
        Some(path) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("cannot open the log {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let has_database = args.database.database_url.is_some();
    let store: Arc<dyn Store> = if let Some(url) = args.database.database_url {
        let store = connect(url).await?;
        if !store.row_security_applies().map_err(|e| e.to_string())? {
            tracing::warn!(
                "the database role is a superuser or bypasses row security: organizations are kept \
                 apart only by the Hub's own queries. Connect as a plain role in production"
            );
        }
        Arc::new(store)
    } else {
        tracing::warn!(
            "no database: everything is kept in memory, no organizer account exists, and the data \
             is lost when the Hub stops"
        );
        Arc::new(MemoryStore::default())
    };
    if args.registry.is_none() && !has_database {
        tracing::warn!(
            "no database and no --registry: the Hub knows no stick and refuses every heartbeat"
        );
    }
    let mut hub = Hub::new(registry, store, log, Box::new(Utc::now));
    if let (Some(account), Some(key)) =
        (&args.download.download_account, &args.download.download_key)
    {
        let download = Download::new(account, &args.download.download_container, key)
            .map_err(|e| e.to_string())?;
        hub = hub.with_download(download);
    }
    let hub = Arc::new(hub);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
    tracing::info!("listening on {}", args.listen);
    axum::serve(listener, router(hub))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| e.to_string())
}

/// Resolves on Ctrl-C, or on SIGTERM, which is what `docker stop` sends.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! { () = ctrl_c => {}, _ = term.recv() => {} }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
