//! `ck-hub`: runs the Hub server, and makes organizer accounts for it.

use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use ck_hub::{
    CreateUserError, Hub, MIN_PASSWORD_LENGTH, MemoryStore, PasswordCost, PostgresStore, Registry,
    Role, Store, create_user, router,
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
    /// Make an organizer account. The password is asked for here and never taken from an argument
    CreateUser(CreateUserArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// Where to listen. Sticks in a QEMU test machine reach the host as 10.0.2.2
    #[arg(long, default_value = "127.0.0.1:8088")]
    listen: SocketAddr,
    /// The registry file ck-forge writes. It holds every stick's secret, so keep it private
    #[arg(long)]
    registry: Option<PathBuf>,
    #[command(flatten)]
    database: DatabaseArgs,
    /// A file to add one line to for every heartbeat, accepted or not. No secrets go in it
    #[arg(long)]
    log: Option<PathBuf>,
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
    /// The name the organizer logs in with
    name: String,
    /// What the organizer may do: `admin` manages everything, `viewer` only looks
    #[arg(long, default_value = "admin")]
    role: String,
    #[command(flatten)]
    database: DatabaseArgs,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_ansi(false).init();
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::CreateUser(args)) => make_user(args).await,
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
    let role = Role::parse(&args.role).ok_or("the role must be admin or viewer")?;
    let url = args
        .database
        .database_url
        .ok_or("accounts need a database: pass --database-url or set CK_DATABASE_URL")?;

    // Typed at a prompt without echo, twice. It is never read from an argument or the environment,
    // so it does not end up in a shell history or a process list.
    let password = rpassword::prompt_password(format!(
        "Password for {} (at least {MIN_PASSWORD_LENGTH} characters): ",
        args.name
    ))
    .map_err(|e| e.to_string())?;
    let again = rpassword::prompt_password("Password again: ").map_err(|e| e.to_string())?;
    if password != again {
        return Err("the two passwords are not the same".to_owned());
    }

    let store = connect(url).await?;
    let name = args.name.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = create_user(&store, PasswordCost::default(), &name, &password, role);
        // The store is dropped on this blocking thread too, where closing the connection is safe.
        drop(store);
        result
    })
    .await
    .map_err(|e| e.to_string())?;
    match result {
        Ok(()) => {
            // Said as a log line, like everything else this program says.
            tracing::info!(role = %args.role, name = %args.name, "the account was made");
            Ok(())
        }
        Err(CreateUserError::BadName) => {
            Err("that name is empty, too long or has control characters".to_owned())
        }
        Err(CreateUserError::PasswordTooShort) => Err(format!(
            "the password must have at least {MIN_PASSWORD_LENGTH} characters"
        )),
        Err(CreateUserError::NameTaken) => {
            Err("an account with that name already exists".to_owned())
        }
        Err(CreateUserError::Unavailable) => Err("the database could not be written".to_owned()),
    }
}

async fn run(args: ServeArgs) -> Result<(), String> {
    let registry_path = args
        .registry
        .ok_or("pass --registry with the file ck-forge writes")?;
    let registry = Registry::open(&registry_path).map_err(|e| e.to_string())?;
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
    let store: Box<dyn Store> = if let Some(url) = args.database.database_url {
        Box::new(connect(url).await?)
    } else {
        tracing::warn!(
            "no database: everything is kept in memory, no organizer account exists, and the data \
             is lost when the Hub stops"
        );
        Box::new(MemoryStore::default())
    };
    let hub = Arc::new(Hub::new(registry, store, log, Box::new(Utc::now)));

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
    tracing::info!("listening on {}", args.listen);
    axum::serve(listener, router(hub))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
