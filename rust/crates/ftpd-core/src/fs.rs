use crate::config::Permissions;
use chrono::{DateTime, Local};
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// FTP operations subject to permission checks (mirrors the original FTP_* flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Download,
    Upload,
    Rename,
    Delete,
    Mkdir,
    List,
}

#[derive(Debug, Error)]
pub enum FsError {
    #[error("not found")]
    NotFound,
    #[error("permission denied")]
    PermissionDenied,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub local: PathBuf,
    /// Normalized absolute virtual path, e.g. "/" or "/pub/docs".
    pub virtual_path: String,
}

/// A per-user filesystem view jailed to the user's home directory.
pub struct VirtualFs {
    root: PathBuf,
    permissions: Permissions,
}

impl VirtualFs {
    pub fn new(root: PathBuf, permissions: Permissions) -> io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            permissions,
        })
    }

    pub fn permissions(&self) -> Permissions {
        self.permissions
    }

    pub fn check(&self, op: Operation) -> Result<(), FsError> {
        if self.permissions.allows(op) {
            Ok(())
        } else {
            Err(FsError::PermissionDenied)
        }
    }

    /// Normalize a client-supplied path against the current virtual directory.
    /// Handles "\", "//", "." and ".."; ".." can never climb above the root.
    fn components(cwd: &str, arg: &str) -> Vec<String> {
        let arg = arg.replace('\\', "/");
        let combined = if arg.starts_with('/') {
            arg
        } else if arg.is_empty() {
            cwd.to_string()
        } else {
            format!("{}/{}", cwd.trim_end_matches('/'), arg)
        };
        let mut out: Vec<String> = Vec::new();
        for seg in combined.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                s => out.push(s.to_string()),
            }
        }
        out
    }

    fn to_local(&self, components: &[String]) -> PathBuf {
        let mut path = self.root.clone();
        for c in components {
            path.push(c);
        }
        path
    }

    /// Canonicalize `path` (or its nearest existing ancestor) and verify the
    /// result stays inside the jail. Defeats symlink escapes; lexically the
    /// path is already guaranteed in-jail by component normalization.
    fn ensure_in_jail(&self, path: &Path) -> Result<(), FsError> {
        let mut cursor = path;
        loop {
            match cursor.canonicalize() {
                Ok(canon) => {
                    if canon.starts_with(&self.root) {
                        return Ok(());
                    }
                    return Err(FsError::NotFound);
                }
                Err(_) => match cursor.parent() {
                    Some(parent) => cursor = parent,
                    None => return Err(FsError::NotFound),
                },
            }
        }
    }

    fn resolve_impl(&self, cwd: &str, arg: &str) -> Resolved {
        let components = Self::components(cwd, arg);
        let virtual_path = if components.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", components.join("/"))
        };
        Resolved {
            local: self.to_local(&components),
            virtual_path,
        }
    }

    /// Resolve a path that must already exist.
    pub fn resolve(&self, cwd: &str, arg: &str) -> Result<Resolved, FsError> {
        let resolved = self.resolve_impl(cwd, arg);
        let meta = std::fs::metadata(&resolved.local).map_err(|_| FsError::NotFound)?;
        if !meta.is_dir() && !meta.is_file() {
            return Err(FsError::NotFound);
        }
        self.ensure_in_jail(&resolved.local)?;
        Ok(resolved)
    }

    /// Resolve an existing regular file (dirs reported as not found, like the original).
    pub fn resolve_file(&self, cwd: &str, arg: &str) -> Result<Resolved, FsError> {
        let resolved = self.resolve(cwd, arg)?;
        if !resolved.local.is_file() {
            return Err(FsError::NotFound);
        }
        Ok(resolved)
    }

    /// Resolve an existing directory.
    pub fn resolve_dir(&self, cwd: &str, arg: &str) -> Result<Resolved, FsError> {
        let resolved = self.resolve(cwd, arg)?;
        if !resolved.local.is_dir() {
            return Err(FsError::NotFound);
        }
        Ok(resolved)
    }

    /// Resolve a path for creation (target itself may not exist yet).
    pub fn resolve_create(&self, cwd: &str, arg: &str) -> Result<Resolved, FsError> {
        let resolved = self.resolve_impl(cwd, arg);
        if resolved.local == self.root {
            return Ok(resolved);
        }
        self.ensure_in_jail(&resolved.local)?;
        Ok(resolved)
    }

    /// Build a Unix-style directory listing, same format as the original:
    /// `-rwx------ 1 user group <size:14> <Mon dd yyyy|Mon dd HH:MM> <name>`
    pub fn list(&self, dir: &Path) -> io::Result<String> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());

        let mut out = String::new();
        for entry in entries {
            let meta = entry
                .metadata()
                .or_else(|_| std::fs::symlink_metadata(entry.path()))?;
            out.push_str(&format_entry(&entry.file_name().to_string_lossy(), &meta));
        }
        Ok(out)
    }
}

pub fn format_entry(name: &str, meta: &std::fs::Metadata) -> String {
    let (kind, size) = if meta.is_dir() {
        ('d', 0)
    } else {
        ('-', meta.len())
    };
    let date = format_date(meta.modified().ok());
    format!("{kind}rwx------ 1 user group {size:>14} {date} {name}\r\n")
}

/// Original rule: older than 356 days shows the year, otherwise HH:MM.
fn format_date(modified: Option<std::time::SystemTime>) -> String {
    let Some(modified) = modified else {
        return "Jan 01 1970 ".to_string();
    };
    let modified: DateTime<Local> = modified.into();
    let age = Local::now().signed_duration_since(modified);
    if age.num_days() > 356 {
        modified.format("%b %d %Y ").to_string()
    } else {
        modified.format("%b %d %H:%M ").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;

    fn jail() -> (tempfile::TempDir, VirtualFs) {
        let tmp = tempfile::tempdir().unwrap();
        stdfs::create_dir_all(tmp.path().join("pub/docs")).unwrap();
        stdfs::write(tmp.path().join("pub/hello.txt"), b"hi").unwrap();
        let vfs = VirtualFs::new(tmp.path().to_path_buf(), Permissions::default()).unwrap();
        (tmp, vfs)
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(VirtualFs::components("/", "a/b"), vec!["a", "b"]);
        assert_eq!(VirtualFs::components("/pub", "../x"), vec!["x"]);
        assert_eq!(VirtualFs::components("/", ".."), Vec::<String>::new());
        assert_eq!(VirtualFs::components("/a/b", ".."), vec!["a"]);
        assert_eq!(VirtualFs::components("/", "a\\b//c/"), vec!["a", "b", "c"]);
        assert_eq!(VirtualFs::components("/pub", ""), vec!["pub"]);
        assert_eq!(VirtualFs::components("/pub", "/abs"), vec!["abs"]);
    }

    #[test]
    fn resolves_existing_paths() {
        let (_tmp, vfs) = jail();
        let r = vfs.resolve_dir("/", "/pub").unwrap();
        assert_eq!(r.virtual_path, "/pub");
        let r = vfs.resolve_file("/pub", "hello.txt").unwrap();
        assert_eq!(r.virtual_path, "/pub/hello.txt");
        assert!(vfs.resolve("/", "/nope").is_err());
    }

    #[test]
    fn cannot_escape_root_via_dotdot() {
        let (_tmp, vfs) = jail();
        let r = vfs.resolve("/", "../../../../etc").unwrap_err();
        assert!(matches!(r, FsError::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn cannot_escape_root_via_symlink() {
        let (tmp, vfs) = jail();
        std::os::unix::fs::symlink("/etc", tmp.path().join("link")).unwrap();
        assert!(vfs.resolve("/", "/link").is_err());
        assert!(vfs.resolve_create("/", "/link/pwned").is_err());
    }

    #[test]
    fn lists_directory_unix_style() {
        let (_tmp, vfs) = jail();
        let out = vfs
            .list(&vfs.resolve_dir("/", "/pub").unwrap().local)
            .unwrap();
        let mut lines = out.lines();
        assert!(lines
            .next()
            .unwrap()
            .starts_with("drwx------ 1 user group "));
        let file_line = lines.next().unwrap();
        assert!(file_line.starts_with("-rwx------ 1 user group "));
        assert!(file_line.ends_with(" hello.txt"));
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn permission_mapping() {
        let p = Permissions::default();
        assert!(p.allows(Operation::Download));
        assert!(p.allows(Operation::List));
        assert!(!p.allows(Operation::Upload));
        assert!(!p.allows(Operation::Rename));
        assert!(!p.allows(Operation::Delete));
        assert!(!p.allows(Operation::Mkdir));
    }
}
