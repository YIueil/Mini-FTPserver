//! End-to-end tests: real server, real control + data connections,
//! driven by a minimal raw-protocol FTP client.

use ftpd_core::auth::UserStore;
use ftpd_core::config::{Permissions, ServerConfig, UserConfig};
use ftpd_core::server::FtpServer;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

fn full_perms() -> Permissions {
    Permissions {
        download: true,
        upload: true,
        rename: true,
        delete: true,
        mkdir: true,
    }
}

async fn start_server(
    root: &Path,
    perms: Permissions,
    max_conn: usize,
    idle_secs: u64,
) -> SocketAddr {
    let config = ServerConfig {
        listen: "127.0.0.1".parse().unwrap(),
        port: 0, // OS-assigned
        max_connections: max_conn,
        idle_timeout_secs: idle_secs,
        users: vec![UserConfig {
            username: "anonymous".into(),
            password: String::new(),
            home: root.to_path_buf(),
            permissions: perms,
        }],
        ..ServerConfig::default()
    };
    let users = UserStore::new(&config.users).unwrap();
    let server = FtpServer::bind(Arc::new(config), Arc::new(users))
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());
    addr
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (reader, writer) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(reader),
            writer,
        };
        let (code, _) = client.read_reply().await;
        assert_eq!(code, 220, "welcome reply");
        client
    }

    async fn read_reply(&mut self) -> (u16, String) {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        let code: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("bad reply line: {line:?}"));
        (code, line.trim_end().to_string())
    }

    async fn cmd(&mut self, c: &str) -> (u16, String) {
        self.writer
            .write_all(format!("{c}\r\n").as_bytes())
            .await
            .unwrap();
        self.read_reply().await
    }

    async fn login(&mut self) {
        assert_eq!(self.cmd("USER anonymous").await.0, 331);
        assert_eq!(self.cmd("PASS whatever").await.0, 230);
    }

    /// Enter PASV and return the connected data stream.
    async fn pasv(&mut self) -> TcpStream {
        let (code, line) = self.cmd("PASV").await;
        assert_eq!(code, 227, "PASV reply: {line}");
        let start = line.find('(').unwrap();
        let end = line.find(')').unwrap();
        let nums: Vec<u16> = line[start + 1..end]
            .split(',')
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(nums.len(), 6);
        let addr: SocketAddr = format!(
            "{}.{}.{}.{}:{}",
            nums[0],
            nums[1],
            nums[2],
            nums[3],
            nums[4] * 256 + nums[5]
        )
        .parse()
        .unwrap();
        TcpStream::connect(addr).await.unwrap()
    }

    /// Enter PORT mode: returns the listener the server will connect to.
    async fn port(&mut self) -> TcpListener {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (p1, p2) = (addr.port() / 256, addr.port() % 256);
        let (code, _) = self.cmd(&format!("PORT 127,0,0,1,{p1},{p2}")).await;
        assert_eq!(code, 200);
        listener
    }
}

async fn read_all(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn full_flow_pasv() {
    let tmp = tempfile::tempdir().unwrap();
    let addr = start_server(tmp.path(), full_perms(), 10, 60).await;
    let mut c = Client::connect(addr).await;

    // Login gate: everything but USER/PASS is rejected while logged out.
    assert_eq!(c.cmd("PWD").await.0, 530);
    assert_eq!(c.cmd("QUIT").await.0, 530);
    c.login().await;

    assert_eq!(c.cmd("PWD").await.1, "257 \"/\" is current directory.");
    assert_eq!(c.cmd("MKD sub").await.0, 250);
    assert_eq!(
        c.cmd("CWD sub").await.1,
        "250 \"/sub\" is current directory."
    );

    // STOR
    let mut data = c.pasv().await;
    assert_eq!(c.cmd("STOR file.txt").await.0, 150);
    data.write_all(b"hello world").await.unwrap();
    data.shutdown().await.unwrap();
    assert_eq!(c.read_reply().await.0, 226);

    assert_eq!(c.cmd("SIZE file.txt").await.1, "213 11");

    // LIST shows the file in Unix format
    let mut data = c.pasv().await;
    assert_eq!(c.cmd("LIST").await.0, 150);
    let listing = String::from_utf8(read_all(&mut data).await).unwrap();
    assert_eq!(c.read_reply().await.0, 226);
    assert!(listing.starts_with("-rwx------ 1 user group "), "{listing}");
    assert!(listing.ends_with(" file.txt\r\n"), "{listing}");

    // LIST with flags is tolerated
    let mut data = c.pasv().await;
    assert_eq!(c.cmd("LIST -la").await.0, 150);
    let listing = String::from_utf8(read_all(&mut data).await).unwrap();
    assert_eq!(c.read_reply().await.0, 226);
    assert!(listing.contains("file.txt"), "{listing}");

    // RETR round-trips the content
    let mut data = c.pasv().await;
    assert_eq!(c.cmd("RETR file.txt").await.0, 150);
    let body = read_all(&mut data).await;
    assert_eq!(c.read_reply().await.0, 226);
    assert_eq!(body, b"hello world");

    // Rename, delete file, walk back up, remove dir
    assert_eq!(c.cmd("RNFR file.txt").await.0, 350);
    assert_eq!(c.cmd("RNTO renamed.txt").await.0, 250);
    let (code, line) = c.cmd("DELE renamed.txt").await;
    assert_eq!(code, 250);
    assert!(line.contains("\"/sub/renamed.txt\""), "{line}");
    assert_eq!(c.cmd("CDUP").await.1, "250 \"/\" is current directory.");
    assert_eq!(c.cmd("RMD sub").await.0, 250);

    assert_eq!(c.cmd("SYST").await.1, "215 UNIX mini-ftpd");
    assert_eq!(c.cmd("NOOP").await.1, "200 OK");
    assert_eq!(c.cmd("FEAT").await.0, 502);
    assert_eq!(c.cmd("PORT 1,2,3").await.0, 501);

    let (code, _) = c.cmd("QUIT").await;
    assert_eq!(code, 220);
}

#[tokio::test]
async fn port_mode_retr() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("data.bin"), b"port mode works").unwrap();
    let addr = start_server(tmp.path(), full_perms(), 10, 60).await;
    let mut c = Client::connect(addr).await;
    c.login().await;

    let listener = c.port().await;
    assert_eq!(c.cmd("RETR data.bin").await.0, 150);
    let (mut data, _) = listener.accept().await.unwrap();
    let body = read_all(&mut data).await;
    assert_eq!(c.read_reply().await.0, 226);
    assert_eq!(body, b"port mode works");
}

#[tokio::test]
async fn permissions_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"readable").unwrap();
    let addr = start_server(tmp.path(), Permissions::default(), 10, 60).await; // download-only
    let mut c = Client::connect(addr).await;
    c.login().await;

    assert_eq!(c.cmd("STOR b.txt").await.1, "550 Permission denied.");
    assert_eq!(c.cmd("DELE a.txt").await.1, "550 Permission denied.");
    assert_eq!(
        c.cmd("MKD d").await.1,
        "550 Can't create directory. Permission denied."
    );
    assert_eq!(c.cmd("RNFR a.txt").await.1, "550 Permission denied.");
    assert_eq!(c.cmd("RMD /").await.1, "550 Permission denied.");
    // Download is allowed.
    assert_eq!(c.cmd("SIZE a.txt").await.1, "213 8");
}

#[tokio::test]
async fn overwrite_requires_delete_permission() {
    let tmp = tempfile::tempdir().unwrap();
    let perms = Permissions {
        download: true,
        upload: true,
        rename: false,
        delete: false,
        mkdir: false,
    };
    let addr = start_server(tmp.path(), perms, 10, 60).await;
    let mut c = Client::connect(addr).await;
    c.login().await;

    let mut data = c.pasv().await;
    assert_eq!(c.cmd("STOR new.txt").await.0, 150);
    data.write_all(b"v1").await.unwrap();
    data.shutdown().await.unwrap();
    assert_eq!(c.read_reply().await.0, 226);

    let _data = c.pasv().await;
    assert_eq!(c.cmd("STOR new.txt").await.1, "550 Permission denied.");
}

#[tokio::test]
async fn traversal_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(tmp.path().join("secret.txt"), b"outside").unwrap();
    let addr = start_server(&root, full_perms(), 10, 60).await;
    let mut c = Client::connect(addr).await;
    c.login().await;

    assert_eq!(c.cmd("RETR ../secret.txt").await.1, "550 File not found.");
    assert_eq!(
        c.cmd("RETR ../../etc/passwd").await.1,
        "550 File not found."
    );
    // ".." at the root stays at the root.
    assert_eq!(c.cmd("CWD ..").await.1, "250 \"/\" is current directory.");
    assert_eq!(c.cmd("PWD").await.1, "257 \"/\" is current directory.");
}

#[tokio::test]
async fn bad_password_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        listen: "127.0.0.1".parse().unwrap(),
        port: 0,
        users: vec![UserConfig {
            username: "alice".into(),
            password: "s3cret".into(),
            home: tmp.path().to_path_buf(),
            permissions: full_perms(),
        }],
        ..ServerConfig::default()
    };
    let users = UserStore::new(&config.users).unwrap();
    let server = FtpServer::bind(Arc::new(config), Arc::new(users))
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run());

    let mut c = Client::connect(addr).await;
    assert_eq!(c.cmd("USER alice").await.0, 331);
    assert_eq!(c.cmd("PASS wrong").await.0, 530);
    // Re-login with the right password.
    assert_eq!(c.cmd("USER alice").await.0, 331);
    assert_eq!(c.cmd("PASS s3cret").await.0, 230);
}

#[tokio::test]
async fn max_connections_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    let addr = start_server(tmp.path(), full_perms(), 1, 60).await;
    let _c1 = Client::connect(addr).await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, _writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        line.trim_end(),
        "421 Too many users are connected, please try again later."
    );
}

#[tokio::test]
async fn idle_timeout_closes_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let addr = start_server(tmp.path(), full_perms(), 10, 1).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, _writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("220"));

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(
        line.trim_end(),
        "426 Connection timed out, aborting transfer"
    );
}

#[tokio::test]
async fn abor_cancels_upload() {
    let tmp = tempfile::tempdir().unwrap();
    let addr = start_server(tmp.path(), full_perms(), 10, 60).await;
    let mut c = Client::connect(addr).await;
    c.login().await;

    let mut data = c.pasv().await;
    assert_eq!(c.cmd("STOR big.bin").await.0, 150);
    // Start sending a large file but never finish it.
    data.write_all(&vec![7u8; 128 * 1024]).await.unwrap();

    assert_eq!(c.cmd("ABOR").await.1, "426 Data connection closed.");
    assert_eq!(c.read_reply().await.1, "226 ABOR command successful.");

    // The session is still usable afterwards.
    assert_eq!(c.cmd("NOOP").await.1, "200 OK");
}
