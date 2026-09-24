use crate::auth::UserStore;
use crate::config::ServerConfig;
use crate::session::Session;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub struct FtpServer {
    listener: TcpListener,
    config: Arc<ServerConfig>,
    users: Arc<UserStore>,
    active: Arc<AtomicUsize>,
}

impl FtpServer {
    pub async fn bind(config: Arc<ServerConfig>, users: Arc<UserStore>) -> io::Result<Self> {
        let listener = TcpListener::bind((config.listen, config.port)).await?;
        Ok(Self {
            listener,
            config,
            users,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Shared counter of currently connected clients (GUI/监控用).
    pub fn active_connections(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active)
    }

    pub async fn run(self) -> io::Result<()> {
        self.run_until(CancellationToken::new()).await
    }

    /// Accept loop with graceful shutdown: cancelling `shutdown` stops
    /// accepting new connections; established sessions run to completion.
    pub async fn run_until(self, shutdown: CancellationToken) -> io::Result<()> {
        info!(
            "mini-ftpd listening on {} (max {} connections)",
            self.local_addr()?,
            self.config.max_connections
        );
        let config = self.config;
        let users = self.users;
        let active = self.active;

        loop {
            let (stream, peer) = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("shutting down, no longer accepting connections");
                    break;
                }
                accepted = self.listener.accept() => accepted?,
            };
            let current = active.fetch_add(1, Ordering::SeqCst);
            if current >= config.max_connections {
                debug!(%peer, "rejected: too many users");
                let mut stream = stream;
                let _ = stream
                    .write_all(b"421 Too many users are connected, please try again later.\r\n")
                    .await;
                active.fetch_sub(1, Ordering::SeqCst);
                continue;
            }

            let config = Arc::clone(&config);
            let users = Arc::clone(&users);
            let active = Arc::clone(&active);
            tokio::spawn(async move {
                info!(%peer, "client connected ({} active)", active.load(Ordering::SeqCst));
                match Session::new(stream, peer, config, users) {
                    Ok(session) => {
                        if let Err(e) = session.run().await {
                            debug!(%peer, "session error: {}", e);
                        }
                    }
                    Err(e) => debug!(%peer, "session init error: {}", e),
                }
                info!(%peer, "client disconnected");
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        Ok(())
    }
}
