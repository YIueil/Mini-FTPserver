use crate::config::PassivePorts;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// How long to wait for the peer to establish the data connection
/// (the original waits 10 seconds).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub enum DataChannel {
    /// PASV mode: we listen, the client connects.
    Passive(TcpListener),
    /// PORT mode: the client listens, we connect.
    Active(SocketAddr),
}

impl DataChannel {
    pub async fn bind_passive(ip: IpAddr, range: PassivePorts) -> io::Result<Self> {
        if range.min == 0 {
            let listener = TcpListener::bind((ip, 0)).await?;
            return Ok(Self::Passive(listener));
        }
        let mut last_err = None;
        for port in range.min..=range.max {
            match TcpListener::bind((ip, port)).await {
                Ok(listener) => return Ok(Self::Passive(listener)),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrInUse, "no passive port available")
        }))
    }

    pub fn passive_port(&self) -> Option<u16> {
        match self {
            Self::Passive(l) => l.local_addr().ok().map(|a| a.port()),
            Self::Active(_) => None,
        }
    }

    pub fn is_passive(&self) -> bool {
        matches!(self, Self::Passive(_))
    }

    /// Establish the data connection (accept or connect), with the
    /// original's 10-second wait.
    pub async fn open(self) -> io::Result<TcpStream> {
        match self {
            Self::Passive(listener) => {
                let (stream, _) = tokio::time::timeout(CONNECT_TIMEOUT, listener.accept())
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "data connection timed out")
                    })??;
                Ok(stream)
            }
            Self::Active(addr) => {
                let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "data connection timed out")
                    })??;
                Ok(stream)
            }
        }
    }
}
