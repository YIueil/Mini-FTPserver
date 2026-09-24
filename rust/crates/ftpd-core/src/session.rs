use crate::auth::UserStore;
use crate::command::{self, FtpCommand};
use crate::config::ServerConfig;
use crate::data::DataChannel;
use crate::fs::{FsError, Operation, VirtualFs};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

const MAX_LINE: usize = 8192;
const TRANSFER_BUF: usize = 64 * 1024;

enum TransferOutcome {
    Complete,
    Aborted,
    Failed(String),
}

struct Transfer {
    cancel: CancellationToken,
    result: oneshot::Receiver<TransferOutcome>,
}

enum Payload {
    Listing(String),
    Download(PathBuf),
    Upload(PathBuf),
}

/// One control connection. Mirrors the original CControlSocket state machine.
pub struct Session {
    config: Arc<ServerConfig>,
    users: Arc<UserStore>,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer: SocketAddr,
    local_ip: IpAddr,
    /// True while STATUS_LOGIN: every command except USER/PASS is rejected,
    /// even QUIT — just like the original.
    awaiting_login: bool,
    login_name: String,
    vfs: Option<VirtualFs>,
    cwd: String,
    data: Option<DataChannel>,
    rename_from: Option<PathBuf>,
    transfer: Option<Transfer>,
}

impl Session {
    pub fn new(
        stream: TcpStream,
        peer: SocketAddr,
        config: Arc<ServerConfig>,
        users: Arc<UserStore>,
    ) -> io::Result<Self> {
        let local_ip = stream.local_addr()?.ip();
        let (reader, writer) = stream.into_split();
        Ok(Self {
            config,
            users,
            reader: BufReader::new(reader),
            writer,
            peer,
            local_ip,
            awaiting_login: true,
            login_name: String::new(),
            vfs: None,
            cwd: "/".to_string(),
            data: None,
            rename_from: None,
            transfer: None,
        })
    }

    pub async fn run(mut self) -> io::Result<()> {
        let welcome = format!("220 {}", self.config.welcome_message);
        if !self.reply(&welcome).await {
            return Ok(());
        }

        loop {
            if self.transfer.is_some() {
                // While a transfer runs, only ABOR/QUIT are processed; the
                // idle timer is suspended (as in the original).
                let reader = &mut self.reader;
                let transfer = self.transfer.as_mut().expect("checked above");
                tokio::select! {
                    outcome = &mut transfer.result => {
                        let outcome = outcome.unwrap_or(TransferOutcome::Aborted);
                        self.transfer = None;
                        self.data = None;
                        self.finish_transfer(outcome).await;
                    }
                    line = read_command(reader) => {
                        match line {
                            Ok(Some(line)) => {
                                let cmd = command::parse(&line);
                                match cmd.verb.as_str() {
                                    "ABOR" => self.do_abor().await,
                                    "QUIT" | "BYE" => {
                                        if let Some(t) = &self.transfer {
                                            t.cancel.cancel();
                                        }
                                        self.say_goodbye().await;
                                        return Ok(());
                                    }
                                    _ => {
                                        self.reply("503 Transfer in progress.").await;
                                    }
                                }
                            }
                            Ok(None) | Err(_) => return Ok(()),
                        }
                    }
                }
            } else {
                let idle = Duration::from_secs(self.config.idle_timeout_secs);
                match tokio::time::timeout(idle, read_command(&mut self.reader)).await {
                    Err(_) => {
                        self.reply("426 Connection timed out, aborting transfer")
                            .await;
                        return Ok(());
                    }
                    Ok(Ok(None)) => return Ok(()),
                    Ok(Ok(Some(line))) => {
                        if !self.dispatch(&line).await {
                            return Ok(());
                        }
                    }
                    Ok(Err(_)) => return Ok(()),
                }
            }
        }
    }

    /// Returns false when the connection should be closed.
    async fn dispatch(&mut self, line: &str) -> bool {
        let cmd = command::parse(line);
        debug!(peer = %self.peer, verb = %cmd.verb, "command");

        if self.awaiting_login && cmd.verb != "USER" && cmd.verb != "PASS" {
            return self.reply("530 Please login with USER and PASS.").await;
        }

        match cmd.verb.as_str() {
            "USER" => {
                self.awaiting_login = true;
                self.login_name = cmd.arg;
                self.reply("331 Please specify the password.").await
            }
            "PASS" => self.do_pass(&cmd).await,
            "QUIT" | "BYE" => {
                self.say_goodbye().await;
                false
            }
            "TYPE" => self.reply(&format!("200 Type set to {}", cmd.arg)).await,
            "PWD" => {
                self.reply(&format!("257 \"{}\" is current directory.", self.cwd))
                    .await
            }
            "CDUP" => self.do_cwd("..").await,
            "CWD" => self.do_cwd(&cmd.arg).await,
            "PORT" => self.do_port(&cmd.arg).await,
            "PASV" => self.do_pasv().await,
            "LIST" => self.do_list(&cmd.arg).await,
            "RETR" => self.do_retr(&cmd.arg).await,
            "STOR" => self.do_stor(&cmd.arg).await,
            "SIZE" => self.do_size(&cmd.arg).await,
            "DELE" => self.do_dele(&cmd.arg).await,
            "RNFR" => self.do_rnfr(&cmd.arg).await,
            "RNTO" => self.do_rnto(&cmd.arg).await,
            "RMD" => self.do_rmd(&cmd.arg).await,
            "MKD" => self.do_mkd(&cmd.arg).await,
            "ABOR" => {
                self.do_abor().await;
                true
            }
            "SYST" => self.reply("215 UNIX mini-ftpd").await,
            "NOOP" => self.reply("200 OK").await,
            _ => self.reply("502 Command not implemented.").await,
        }
    }

    async fn do_pass(&mut self, cmd: &FtpCommand) -> bool {
        if self.login_name.is_empty() {
            return self.reply("503 Login with USER first.").await;
        }
        let Some(user) = self.users.authenticate(&self.login_name, &cmd.arg) else {
            return self
                .reply("530 Not logged in, user or password incorrect!")
                .await;
        };
        match VirtualFs::new(user.home.clone(), user.permissions) {
            Ok(vfs) => {
                info!(peer = %self.peer, user = %user.username, "logged in");
                self.vfs = Some(vfs);
                self.cwd = "/".to_string();
                self.awaiting_login = false;
                self.reply("230 Login successful.").await
            }
            Err(_) => {
                self.reply("530 Not logged in, user or password incorrect!")
                    .await
            }
        }
    }

    async fn do_cwd(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        match vfs.resolve_dir(&self.cwd, arg) {
            Err(_) => {
                self.reply(&format!("550 \"{}\": Directory not found.", arg))
                    .await
            }
            Ok(res) => {
                if vfs.check(Operation::List).is_err() {
                    return self
                        .reply(&format!("550 \"{}\": Permission denied.", arg))
                        .await;
                }
                self.cwd = res.virtual_path;
                self.reply(&format!("250 \"{}\" is current directory.", self.cwd))
                    .await
            }
        }
    }

    async fn do_port(&mut self, arg: &str) -> bool {
        let parts: Vec<&str> = arg.split(',').collect();
        let nums: Vec<u8> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        if parts.len() != 6 || nums.len() != 6 {
            return self.reply("501 Illegal PORT command.").await;
        }
        let ip = Ipv4Addr::new(nums[0], nums[1], nums[2], nums[3]);
        let port = nums[4] as u16 * 256 + nums[5] as u16;
        self.data = Some(DataChannel::Active(SocketAddr::new(IpAddr::V4(ip), port)));
        self.reply("200 Port command successful.").await
    }

    async fn do_pasv(&mut self) -> bool {
        self.data = None;
        let IpAddr::V4(ip) = self.local_ip else {
            // PASV cannot express IPv6 addresses (the original is IPv4-only too)
            return self.reply("421 Failed to create socket.").await;
        };
        match DataChannel::bind_passive(self.local_ip, self.config.passive_ports).await {
            Err(_) => self.reply("421 Failed to create socket.").await,
            Ok(channel) => {
                let port = channel.passive_port().unwrap_or(0);
                self.data = Some(channel);
                let o = ip.octets();
                self.reply(&format!(
                    "227 Entering Passive Mode ({},{},{},{},{},{}).",
                    o[0],
                    o[1],
                    o[2],
                    o[3],
                    port / 256,
                    port % 256
                ))
                .await
            }
        }
    }

    async fn do_list(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        // Tolerate common client flags ("LIST -la [path]"); the original
        // treats the whole argument as a path and fails with 550.
        let path_arg: String = arg
            .split_whitespace()
            .filter(|token| !token.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");
        let resolved = match vfs.resolve(&self.cwd, &path_arg) {
            Err(FsError::NotFound) | Err(FsError::Io(_)) => {
                return self
                    .reply(&format!("550 \"{}\": Directory not found.", arg))
                    .await;
            }
            Err(FsError::PermissionDenied) => {
                return self
                    .reply(&format!("550 \"{}\": Permission denied.", arg))
                    .await;
            }
            Ok(res) => res,
        };
        if vfs.check(Operation::List).is_err() {
            return self
                .reply(&format!("550 \"{}\": Permission denied.", arg))
                .await;
        }
        let listing = if resolved.local.is_dir() {
            match vfs.list(&resolved.local) {
                Ok(l) => l,
                Err(_) => {
                    return self
                        .reply(&format!("550 \"{}\": Directory not found.", arg))
                        .await
                }
            }
        } else {
            let name = resolved
                .local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            match std::fs::metadata(&resolved.local) {
                Ok(meta) => crate::fs::format_entry(&name, &meta),
                Err(_) => String::new(),
            }
        };

        if !self
            .reply("150 Opening ASCII mode data connection for directory list.")
            .await
        {
            return false;
        }
        match self.open_data().await {
            None => true,
            Some(stream) => {
                if listing.is_empty() {
                    drop(stream);
                    self.data = None;
                    self.reply("226 Transfer complete.").await
                } else {
                    self.start_transfer(Payload::Listing(listing), stream);
                    true
                }
            }
        }
    }

    async fn do_retr(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let resolved = match vfs.resolve_file(&self.cwd, arg) {
            Err(FsError::NotFound) | Err(FsError::Io(_)) => {
                return self.reply("550 File not found.").await;
            }
            Err(FsError::PermissionDenied) => {
                return self.reply("550 Permission denied.").await;
            }
            Ok(res) => res,
        };
        if vfs.check(Operation::Download).is_err() {
            return self.reply("550 Permission denied.").await;
        }
        if !self
            .reply("150 Opening BINARY mode data connection for file transfer.")
            .await
        {
            return false;
        }
        match self.open_data().await {
            None => true,
            Some(stream) => {
                self.start_transfer(Payload::Download(resolved.local), stream);
                true
            }
        }
    }

    async fn do_stor(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        if arg.is_empty() {
            return self.reply("550 Filename invalid.").await;
        }
        let resolved = match vfs.resolve_create(&self.cwd, arg) {
            Ok(res) => res,
            Err(_) => return self.reply("550 Filename invalid.").await,
        };
        // Original: overwriting an existing file requires delete permission.
        if resolved.local.is_file() && !vfs.permissions().allows(Operation::Delete) {
            return self.reply("550 Permission denied.").await;
        }
        if vfs.check(Operation::Upload).is_err() {
            return self.reply("550 Permission denied.").await;
        }
        if !self
            .reply("150 Opening BINARY mode data connection for file transfer.")
            .await
        {
            return false;
        }
        match self.open_data().await {
            None => true,
            Some(stream) => {
                self.start_transfer(Payload::Upload(resolved.local), stream);
                true
            }
        }
    }

    async fn do_size(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        match vfs.resolve_file(&self.cwd, arg) {
            Err(FsError::NotFound) | Err(FsError::Io(_)) => self.reply("550 File not found.").await,
            Err(FsError::PermissionDenied) => self.reply("550 Permission denied.").await,
            Ok(res) => {
                if vfs.check(Operation::Download).is_err() {
                    return self.reply("550 Permission denied.").await;
                }
                let size = std::fs::metadata(&res.local).map(|m| m.len()).unwrap_or(0);
                self.reply(&format!("213 {}", size)).await
            }
        }
    }

    async fn do_dele(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let resolved = match vfs.resolve_file(&self.cwd, arg) {
            Err(FsError::NotFound) | Err(FsError::Io(_)) => {
                return self.reply("550 File not found.").await;
            }
            Err(FsError::PermissionDenied) => {
                return self.reply("550 Permission denied.").await;
            }
            Ok(res) => res,
        };
        if vfs.check(Operation::Delete).is_err() {
            return self.reply("550 Permission denied.").await;
        }
        match std::fs::remove_file(&resolved.local) {
            Ok(_) => {
                info!(peer = %self.peer, path = %resolved.virtual_path, "file deleted");
                self.reply(&format!(
                    "250 File \"{}\" was deleted successfully.",
                    resolved.virtual_path
                ))
                .await
            }
            Err(_) => {
                self.reply(&format!(
                    "450 Internal error deleting the file: \"{}\".",
                    resolved.virtual_path
                ))
                .await
            }
        }
    }

    async fn do_rnfr(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let rename_ok = vfs.permissions().allows(Operation::Rename);
        // Original order: try file first, then fall back to directory.
        if let Ok(res) = vfs.resolve_file(&self.cwd, arg) {
            if !rename_ok {
                return self.reply("550 Permission denied.").await;
            }
            self.rename_from = Some(res.local);
            return self
                .reply("350 File exists, ready for destination name.")
                .await;
        }
        match vfs.resolve_dir(&self.cwd, arg) {
            Ok(res) => {
                if !rename_ok {
                    return self.reply("550 Permission denied.").await;
                }
                self.rename_from = Some(res.local);
                self.reply("350 Directory exists, ready for destination name.")
                    .await
            }
            Err(FsError::PermissionDenied) => self.reply("550 Permission denied.").await,
            Err(_) => self.reply("550 Directory not found.").await,
        }
    }

    async fn do_rnto(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let Some(from) = self.rename_from.take() else {
            return self.reply("450 Internal error renamed the file.").await;
        };
        let Ok(target) = vfs.resolve_create(&self.cwd, arg) else {
            return self.reply("450 Internal error renamed the file.").await;
        };
        match std::fs::rename(&from, &target.local) {
            Ok(_) => {
                info!(peer = %self.peer, to = %target.virtual_path, "renamed");
                self.reply("250 renamed successfully.").await
            }
            Err(_) => self.reply("450 Internal error renamed the file.").await,
        }
    }

    async fn do_rmd(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let resolved = match vfs.resolve(&self.cwd, arg) {
            Err(_) => return self.reply("550 Directory not found.").await,
            Ok(res) => res,
        };
        if vfs.check(Operation::Delete).is_err() || resolved.virtual_path == "/" {
            // The original happily deletes the user's home directory; we don't.
            return self.reply("550 Permission denied.").await;
        }
        match std::fs::remove_dir(&resolved.local) {
            Ok(_) => {
                info!(peer = %self.peer, path = %resolved.virtual_path, "directory deleted");
                self.reply("250 Directory deleted successfully.").await
            }
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {
                self.reply("550 Directory not empty.").await
            }
            Err(_) => {
                self.reply("450 Internal error deleting the directory.")
                    .await
            }
        }
    }

    async fn do_mkd(&mut self, arg: &str) -> bool {
        let Some(vfs) = &self.vfs else {
            return self.reply("530 Please login with USER and PASS.").await;
        };
        let Ok(resolved) = vfs.resolve_create(&self.cwd, arg) else {
            return self
                .reply("450 Internal error creating the directory.")
                .await;
        };
        if vfs.check(Operation::Mkdir).is_err() {
            return self
                .reply("550 Can't create directory. Permission denied.")
                .await;
        }
        if resolved.local.exists() {
            return self.reply("550 Directory already exists.").await;
        }
        // create_dir_all matches the original's MakeSureDirectoryPathExists.
        match std::fs::create_dir_all(&resolved.local) {
            Ok(_) => {
                info!(peer = %self.peer, path = %resolved.virtual_path, "directory created");
                self.reply("250 Directory created successfully.").await
            }
            Err(_) => {
                self.reply("450 Internal error creating the directory.")
                    .await
            }
        }
    }

    async fn do_abor(&mut self) {
        if self.transfer.is_some() {
            if let Some(t) = &self.transfer {
                t.cancel.cancel();
            }
            self.reply("426 Data connection closed.").await;
        }
        self.reply("226 ABOR command successful.").await;
    }

    async fn say_goodbye(&mut self) {
        let msg = format!("220 {}", self.config.goodbye_message);
        self.reply(&msg).await;
    }

    /// 150 has already been sent; establish the data connection.
    async fn open_data(&mut self) -> Option<TcpStream> {
        let Some(channel) = self.data.take() else {
            self.reply("425 Can't open data connection.").await;
            return None;
        };
        let passive = channel.is_passive();
        match channel.open().await {
            Ok(stream) => Some(stream),
            Err(_) if passive => {
                self.reply("421 Failed to create data connection socket.")
                    .await;
                None
            }
            Err(_) => {
                self.reply("425 Can't open data connection.").await;
                None
            }
        }
    }

    fn start_transfer(&mut self, payload: Payload, stream: TcpStream) {
        let cancel = CancellationToken::new();
        let (tx, rx) = oneshot::channel();
        let token = cancel.clone();
        let peer = self.peer;
        tokio::spawn(async move {
            let outcome = tokio::select! {
                _ = token.cancelled() => TransferOutcome::Aborted,
                result = run_transfer(payload, stream) => result,
            };
            debug!(peer = %peer, "transfer task finished");
            let _ = tx.send(outcome);
        });
        self.transfer = Some(Transfer { cancel, result: rx });
    }

    async fn finish_transfer(&mut self, outcome: TransferOutcome) {
        match outcome {
            TransferOutcome::Complete => {
                self.reply("226 Transfer complete").await;
            }
            TransferOutcome::Failed(msg) => {
                self.reply(&msg).await;
            }
            TransferOutcome::Aborted => {
                // ABOR/QUIT path has already replied.
            }
        }
    }

    async fn reply(&mut self, msg: &str) -> bool {
        debug!(peer = %self.peer, "<-- {}", msg);
        let mut line = String::with_capacity(msg.len() + 2);
        line.push_str(msg);
        line.push_str("\r\n");
        self.writer.write_all(line.as_bytes()).await.is_ok()
    }
}

async fn read_command(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(256);
    let n = tokio::io::AsyncBufReadExt::read_until(reader, b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command line too long",
        ));
    }
    let line = String::from_utf8_lossy(&buf);
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

async fn run_transfer(payload: Payload, mut stream: TcpStream) -> TransferOutcome {
    const ABORTED: &str = "426 Connection closed; transfer aborted.";
    const CANT_ACCESS: &str = "450 Can't access file.";
    let mut buf = vec![0u8; TRANSFER_BUF];
    match payload {
        Payload::Listing(text) => match stream.write_all(text.as_bytes()).await {
            Ok(_) => TransferOutcome::Complete,
            Err(_) => TransferOutcome::Failed(ABORTED.into()),
        },
        Payload::Download(path) => {
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(_) => return TransferOutcome::Failed(ABORTED.into()),
            };
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => return TransferOutcome::Complete,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() {
                            return TransferOutcome::Failed(ABORTED.into());
                        }
                    }
                    Err(_) => return TransferOutcome::Failed(ABORTED.into()),
                }
            }
        }
        Payload::Upload(path) => {
            // File::create truncates — the original used modeNoTruncate and
            // left garbage tails when overwriting a longer file.
            let mut file = match tokio::fs::File::create(&path).await {
                Ok(f) => f,
                Err(_) => return TransferOutcome::Failed(CANT_ACCESS.into()),
            };
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => {
                        return match file.flush().await {
                            Ok(_) => TransferOutcome::Complete,
                            Err(_) => TransferOutcome::Failed(CANT_ACCESS.into()),
                        };
                    }
                    Ok(n) => {
                        if file.write_all(&buf[..n]).await.is_err() {
                            return TransferOutcome::Failed(CANT_ACCESS.into());
                        }
                    }
                    Err(_) => return TransferOutcome::Failed(ABORTED.into()),
                }
            }
        }
    }
}
