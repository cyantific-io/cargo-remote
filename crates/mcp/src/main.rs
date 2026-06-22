//! `rustle-mcp`: an MCP server (stdio transport) exposing remote cargo builds as tools.
//!
//! `main` only bootstraps: parse clap args, set up logging to stderr (stdout is the JSON-RPC
//! channel), build the server, and serve over stdio until the client disconnects.

mod server;

use clap::{Parser, ValueEnum};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use rustle_core::outbound::{SyncMode, DEFAULT_CONCURRENCY};
use server::RustleServer;

/// Logging verbosity, set via `--log-level` (no env vars).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

/// How a push reconciles remote state, set via `--sync-mode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, ValueEnum)]
enum SyncModeArg {
    Sftp,
    Agent,
    #[default]
    Auto,
}

impl From<SyncModeArg> for SyncMode {
    fn from(value: SyncModeArg) -> Self {
        match value {
            SyncModeArg::Sftp => SyncMode::Sftp,
            SyncModeArg::Agent => SyncMode::Agent,
            SyncModeArg::Auto => SyncMode::Auto,
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "rustle-mcp", version, about = "MCP server for remote cargo builds")]
struct Args {
    /// Max concurrent file transfers.
    #[arg(short = 'j', long = "jobs", default_value_t = DEFAULT_CONCURRENCY)]
    jobs: usize,

    /// How a push reconciles remote state: agent (deploy a remote helper, one round-trip),
    /// sftp (native listing), or auto (agent, falling back to sftp).
    #[arg(long = "sync-mode", value_enum, default_value_t = SyncModeArg::Auto)]
    sync_mode: SyncModeArg,

    /// Log verbosity (written to stderr).
    #[arg(long = "log-level", value_enum, default_value_t = LogLevel::Info)]
    log_level: LogLevel,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Logs MUST go to stderr — stdout carries the MCP JSON-RPC protocol.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(args.log_level.as_filter()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("starting rustle MCP server on stdio");
    let service = RustleServer::new(args.jobs, args.sync_mode.into())
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
