//! `ck-hub`: runs the Hub server.

use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use ck_hub::{Hub, MemoryStore, PostgresStore, Registry, Store, router};
use clap::Parser;

#[derive(Parser)]
#[command(name = "ck-hub", version, about = "The Contest Keeper server")]
struct Cli {
    /// Where to listen. Sticks in a QEMU test machine reach the host as 10.0.2.2
    #[arg(long, default_value = "127.0.0.1:8088")]
    listen: SocketAddr,
    /// The registry file ck-forge writes. It holds every stick's secret, so keep it private
    #[arg(long)]
    registry: PathBuf,
    /// The PostgreSQL address, like `postgres://user:password@localhost/ckhub`. Without it the Hub
    /// keeps everything in memory and forgets it when it stops
    #[arg(long, env = "CK_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    /// A file to add one line to for every heartbeat, accepted or not. No secrets go in it
    #[arg(long)]
    log: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt().with_ansi(false).init();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let registry = Registry::open(&cli.registry).map_err(|e| e.to_string())?;
    let log = match &cli.log {
        Some(path) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("cannot open the log {}: {e}", path.display()))?,
        ),
        None => None,
    };
    let store: Box<dyn Store> = if let Some(url) = cli.database_url.clone() {
        // The database client blocks and starts a runtime of its own, so it must not connect on a
        // thread that already runs async tasks.
        let connected = tokio::task::spawn_blocking(move || PostgresStore::connect(&url))
            .await
            .map_err(|e| e.to_string())?;
        Box::new(connected.map_err(|e| e.to_string())?)
    } else {
        tracing::warn!("no database: everything is kept in memory and lost when the Hub stops");
        Box::new(MemoryStore::default())
    };
    let hub = Arc::new(Hub::new(registry, store, log, Box::new(Utc::now)));

    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", cli.listen))?;
    tracing::info!("listening on {}", cli.listen);
    axum::serve(listener, router(hub))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
