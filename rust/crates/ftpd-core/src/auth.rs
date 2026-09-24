use crate::config::{Permissions, UserConfig};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("home directory for user \"{username}\" does not exist: {path}")]
    HomeNotFound { username: String, path: PathBuf },
    #[error("duplicate username: {0}")]
    DuplicateUser(String),
}

#[derive(Debug, Clone)]
pub struct User {
    pub username: String,
    pub home: PathBuf,
    pub permissions: Permissions,
    password: String,
    anonymous: bool,
}

pub struct UserStore {
    users: Vec<User>,
}

impl UserStore {
    pub fn new(configs: &[UserConfig]) -> Result<Self, AuthError> {
        let mut users = Vec::with_capacity(configs.len());
        for cfg in configs {
            if users
                .iter()
                .any(|u: &User| u.username.eq_ignore_ascii_case(&cfg.username))
            {
                return Err(AuthError::DuplicateUser(cfg.username.clone()));
            }
            let home = cfg
                .home
                .canonicalize()
                .map_err(|_| AuthError::HomeNotFound {
                    username: cfg.username.clone(),
                    path: cfg.home.clone(),
                })?;
            users.push(User {
                anonymous: cfg.username.eq_ignore_ascii_case("anonymous"),
                username: cfg.username.clone(),
                password: cfg.password.clone(),
                home,
                permissions: cfg.permissions,
            });
        }
        Ok(Self { users })
    }

    /// Mirrors the original: password must match, except for `anonymous`
    /// which is accepted with any password.
    pub fn authenticate(&self, username: &str, password: &str) -> Option<User> {
        let user = self
            .users
            .iter()
            .find(|u| u.username.eq_ignore_ascii_case(username))?;
        if user.anonymous || user.password == password {
            Some(user.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UserStore {
        let dir = std::env::temp_dir();
        UserStore::new(&[
            UserConfig {
                username: "anonymous".into(),
                password: String::new(),
                home: dir.clone(),
                permissions: Permissions::default(),
            },
            UserConfig {
                username: "Alice".into(),
                password: "pw".into(),
                home: dir,
                permissions: Permissions::default(),
            },
        ])
        .unwrap()
    }

    #[test]
    fn anonymous_accepts_any_password() {
        let s = store();
        assert!(s.authenticate("anonymous", "whatever").is_some());
        assert!(s.authenticate("ANONYMOUS", "").is_some());
    }

    #[test]
    fn regular_user_needs_password() {
        let s = store();
        assert!(s.authenticate("alice", "pw").is_some());
        assert!(s.authenticate("alice", "bad").is_none());
        assert!(s.authenticate("mallory", "pw").is_none());
    }
}
