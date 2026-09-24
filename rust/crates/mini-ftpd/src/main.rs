use anyhow::{Context, Result};
use clap::Parser;
use ftpd_core::auth::UserStore;
use ftpd_core::config::ServerConfig;
use ftpd_core::server::FtpServer;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// A mini FTP server — Rust rewrite of the classic Slyar FTPserver (MFC).
#[derive(Parser)]
#[command(name = "mini-ftpd", version, about, long_about = None)]
struct Args {
    /// Path to the TOML configuration file.
    /// Without one, built-in defaults are used: port 21, user "anonymous"
    /// (any password), home ./ftp_root, download-only.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Override the listen port.
    #[arg(short, long, value_name = "PORT")]
    port: Option<u16>,

    /// Override the listen address.
    #[arg(short, long, value_name = "ADDR")]
    listen: Option<IpAddr>,

    /// Override the home directory of the first configured user.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let mut config = match &args.config {
        Some(path) => ServerConfig::load(path)
            .with_context(|| format!("cannot load config {}", path.display()))?,
        None => {
            tracing::info!(
                "no --config given, using built-in defaults (anonymous, ./ftp_root, download-only)"
            );
            std::fs::create_dir_all("./ftp_root").ok();
            ServerConfig::default()
        }
    };

    if let Some(port) = args.port {
        config.port = port;
    }
    if let Some(listen) = args.listen {
        config.listen = listen;
    }
    if let Some(root) = args.root {
        if let Some(first) = config.users.first_mut() {
            first.home = root;
        }
    }
    config.validate()?;

    let users = UserStore::new(&config.users)?;
    let server = FtpServer::bind(Arc::new(config), Arc::new(users)).await?;
    tracing::info!("serving FTP on {}", server.local_addr()?);

    server.run().await?;
    Ok(())
}
