// SPDX-License-Identifier: AGPL-3.0-only

//! Users, teams, rights, tokens and console sessions.
//!
//! Rights: a server admin may do everything. Others get a role per
//! repository (read < write < admin) from grants to themselves or their
//! teams; a grant on `*` covers every repository. A public repository
//! may be read by anyone. A token acts for its user, limited to the
//! repositories it names (all if none) and to reading unless it may
//! write.

pub mod keys;
pub mod oidc;
pub mod rsa_key;
pub mod usign_key;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use pkgpod_base::db::Db;
use pkgpod_base::{now, random_hex};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Result<Role> {
        Ok(match s {
            "read" => Role::Read,
            "write" => Role::Write,
            "admin" => Role::Admin,
            _ => {
                return Err(Error::Invalid(format!(
                    "unknown role {s:?}: read, write or admin"
                )));
            }
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct User {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    pub admin: bool,
    pub disabled: bool,
    pub created: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Team {
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    pub repo: String,
    /// `user:<name>` or `team:<name>`.
    pub subject: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub user: String,
    pub name: String,
    /// Repositories it may be used for; empty: all the user's.
    pub repos: Vec<String>,
    pub write: bool,
    pub expires: Option<i64>,
    pub created: i64,
    pub used: Option<i64>,
}

/// What a token limits its user to.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub repos: Vec<String>,
    pub write: bool,
    /// Given for a CI job's OIDC token (a trust rule): it changes no
    /// trust rule, whatever its user may.
    pub exchanged: bool,
    /// A publisher token, of a trust rule without a user ("trust:<rule>"):
    /// it writes (or reads) its repositories and nothing else, whoever
    /// is granted what.
    pub publisher: Option<String>,
    /// When the token stops working, if it does: a token it makes
    /// stops then too.
    pub expires: Option<i64>,
}

/// Who makes a request.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    pub user: Option<User>,
    pub scope: Option<Scope>,
}

impl Caller {
    pub fn anonymous() -> Caller {
        Caller::default()
    }

    pub fn name(&self) -> &str {
        match (&self.user, &self.scope) {
            (Some(u), _) => u.name.as_str(),
            (
                None,
                Some(Scope {
                    publisher: Some(p), ..
                }),
            ) => p.as_str(),
            _ => "anonymous",
        }
    }

    /// A server admin, not limited by a token.
    pub fn is_admin(&self) -> bool {
        self.user.as_ref().is_some_and(|u| u.admin)
            && self
                .scope
                .as_ref()
                .is_none_or(|s| s.repos.is_empty() && s.write)
    }
}

/// Token prefix: lets a password field tell a token from a password.
pub const TOKEN_PREFIX: &str = "pkgpod_";

pub fn check_user_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
    {
        return Err(Error::Invalid(format!(
            "invalid name {name:?}: letters, digits, '-', '_', '.', '@'"
        )));
    }
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.chars().count() < 8 {
        return Err(Error::Invalid(
            "a password needs at least 8 characters".into(),
        ));
    }
    // A login reads such a password as a token, so it could never log in.
    if password.starts_with(TOKEN_PREFIX) {
        return Err(Error::Invalid(format!(
            "a password may not begin with {TOKEN_PREFIX}: tokens do"
        )));
    }
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| Error::Invalid(format!("hashing the password: {e}")))
}

/// Password checks at once: each takes ~19 MiB and a fraction of a
/// second on purpose, so a flood of guesses waits its turn instead of
/// taking the machine.
const CHECKS_AT_ONCE: usize = 4;

/// How long a check waits for its turn before it counts as failed.
const CHECK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

fn verify_password(password: &str, hash: &str) -> bool {
    static SLOTS: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());
    let (lock, cv) = &SLOTS;
    {
        let busy = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut busy, to) = cv
            .wait_timeout_while(busy, CHECK_WAIT, |b| *b >= CHECKS_AT_ONCE)
            .unwrap_or_else(|e| e.into_inner());
        if to.timed_out() && *busy >= CHECKS_AT_ONCE {
            return false;
        }
        *busy += 1;
    }
    let ok = PasswordHash::new(hash).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    });
    *lock.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    cv.notify_one();
    ok
}

/// Hash of a password nobody has, checked for unknown users so a failed
/// login takes the same time either way.
fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| hash_password(&random_hex(16)).expect("hashing"))
}

fn sha(s: &str) -> Vec<u8> {
    Sha256::digest(s.as_bytes()).to_vec()
}

pub struct Auth {
    db: Arc<Db>,
    /// Digests of (user, hash, password) already verified: apt sends many
    /// requests and argon2 is slow on purpose.
    verified: Mutex<HashSet<[u8; 32]>>,
    failed: Mutex<Failed>,
}

/// Failed logins by (user, who, where), each counted in an audit row.
type Failed = HashMap<(String, &'static str, &'static str), Failures>;

/// Failed logins of a user (or of unknown users, or of tokens) at one
/// place.
struct Failures {
    /// The audit row counting them.
    id: i64,
    /// When the first was.
    first: i64,
    n: u64,
    /// The count the row says, and when it was written.
    shown: u64,
    written: i64,
}

/// Failed logins of a user at one place within this many seconds of
/// the first are counted in one audit row.
const FAILED_ROW_SECS: i64 = 3600;

/// After the first few, a row's count is written at most this often:
/// a token or a disabled user is refused without a password check, so
/// failures can come as fast as requests do.
const FAILED_WRITE_SECS: i64 = 10;

/// What a failed-login audit row says: where, how many times, and who
/// unless it names the user.
fn failed_detail(who: &str, surface: &str, n: u64) -> String {
    let times = if n == 1 { "time" } else { "times" };
    if who.is_empty() {
        format!("{surface}, {n} {times}")
    } else {
        format!("{surface}, {n} {times}, {who}")
    }
}

const USER_COLS: &str = "id, name, admin, disabled, created";

fn row_user(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        admin: r.get(2)?,
        disabled: r.get(3)?,
        created: r.get(4)?,
    })
}

impl Auth {
    pub fn new(db: Arc<Db>) -> Auth {
        Auth {
            db,
            verified: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashMap::new()),
        }
    }

    // ---- users ----

    pub fn users(&self) -> Result<Vec<User>> {
        Ok(self.db.read(|c| {
            let mut st = c.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY name"))?;
            let v = st
                .query_map([], row_user)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    pub fn user(&self, name: &str) -> Result<User> {
        self.db
            .read(|c| {
                c.query_row(
                    &format!("SELECT {USER_COLS} FROM users WHERE name = ?1"),
                    [name],
                    row_user,
                )
                .optional()
            })?
            .ok_or_else(|| Error::NotFound(format!("no user {name}")))
    }

    pub fn create_user(&self, name: &str, password: &str, admin: bool) -> Result<User> {
        check_user_name(name)?;
        let hash = hash_password(password)?;
        self.db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO users (name, password, admin, created) VALUES (?1, ?2, ?3, ?4)",
                    params![name, hash, admin, now()],
                )
            })
            .map_err(|e| unique(e, &format!("user {name} exists")))?;
        self.user(name)
    }

    pub fn set_password(&self, name: &str, password: &str) -> Result<()> {
        let hash = hash_password(password)?;
        let n = self.db.write(|tx| {
            tx.execute(
                "UPDATE users SET password = ?1 WHERE name = ?2",
                params![hash, name],
            )
        })?;
        if n == 0 {
            return Err(Error::NotFound(format!("no user {name}")));
        }
        // Sessions end with a password change.
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM sessions WHERE user = (SELECT id FROM users WHERE name = ?1)",
                [name],
            )
        })?;
        Ok(())
    }

    /// Changes the admin and disabled flags; the last enabled admin stays.
    pub fn set_flags(
        &self,
        name: &str,
        admin: Option<bool>,
        disabled: Option<bool>,
    ) -> Result<User> {
        let u = self.user(name)?;
        let admin = admin.unwrap_or(u.admin);
        let disabled = disabled.unwrap_or(u.disabled);
        if u.admin && !u.disabled && (!admin || disabled) {
            self.keep_an_admin(&u)?;
        }
        self.db.write(|tx| {
            tx.execute(
                "UPDATE users SET admin = ?1, disabled = ?2 WHERE id = ?3",
                params![admin, disabled, u.id],
            )?;
            if disabled {
                tx.execute("DELETE FROM sessions WHERE user = ?1", [u.id])?;
            }
            Ok::<_, rusqlite::Error>(())
        })?;
        self.user(name)
    }

    fn keep_an_admin(&self, u: &User) -> Result<()> {
        let others: i64 = self.db.read(|c| {
            c.query_row(
                "SELECT count(*) FROM users WHERE admin AND NOT disabled AND id != ?1",
                [u.id],
                |r| r.get(0),
            )
        })?;
        if others == 0 {
            return Err(Error::Invalid(format!(
                "{} is the last admin; make another user admin first",
                u.name
            )));
        }
        Ok(())
    }

    pub fn delete_user(&self, name: &str) -> Result<()> {
        let u = self.user(name)?;
        if u.admin && !u.disabled {
            self.keep_an_admin(&u)?;
        }
        self.db
            .write(|tx| tx.execute("DELETE FROM users WHERE id = ?1", [u.id]))?;
        Ok(())
    }

    /// Creates `admin` with a random password if there are no users,
    /// writing the password to `file` (mode 0600). Returns whether it did.
    pub fn bootstrap(&self, file: &Path) -> Result<bool> {
        if !self.users()?.is_empty() {
            return Ok(false);
        }
        let pw = random_hex(12);
        self.create_user("admin", &pw, true)?;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(file)
            .map_err(|e| Error::Invalid(format!("{}: {e}", file.display())))?;
        writeln!(f, "{pw}").map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(true)
    }

    // ---- authentication ----

    /// Checks a user name and password (or token), given at `surface`
    /// (api, console, serving); a failure is logged and audited.
    pub fn login(&self, name: &str, password: &str, surface: &'static str) -> Option<Caller> {
        let (c, who) = self.check_login(name, password);
        if c.is_none() {
            self.login_failed(name, who, surface);
        }
        c
    }

    /// The caller, and for the audit who `name` is: "" a user, else
    /// "unknown users" or "tokens".
    fn check_login(&self, name: &str, password: &str) -> (Option<Caller>, &'static str) {
        if password.starts_with(TOKEN_PREFIX) {
            return (self.token_login(name, password), "tokens");
        }
        let row: Option<(User, String)> = match self.db.read(|c| {
            c.query_row(
                &format!("SELECT {USER_COLS}, password FROM users WHERE name = ?1"),
                [name],
                |r| Ok((row_user(r)?, r.get(5)?)),
            )
            .optional()
        }) {
            Ok(r) => r,
            Err(_) => return (None, "unknown users"),
        };
        let Some((user, hash)) = row else {
            verify_password(password, dummy_hash());
            return (None, "unknown users");
        };
        if user.disabled {
            return (None, "");
        }
        let key: [u8; 32] = {
            let mut h = Sha256::new();
            for part in [name, &hash, password] {
                h.update(part.as_bytes());
                h.update([0]);
            }
            h.finalize().into()
        };
        let seen = self
            .verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key);
        if !seen {
            if !verify_password(password, &hash) {
                return (None, "");
            }
            let mut v = self.verified.lock().unwrap_or_else(|e| e.into_inner());
            if v.len() >= 4096 {
                v.clear();
            }
            v.insert(key);
        }
        (
            Some(Caller {
                user: Some(user),
                scope: None,
            }),
            "",
        )
    }

    /// A failed login: a warning in the log, and an audit row per user and
    /// place that counts them (unknown names share one, tokens another, so
    /// guesses at names do not fill the audit log).
    fn login_failed(&self, name: &str, who: &'static str, surface: &'static str) {
        let shown: String = name.chars().take(64).collect();
        tracing::warn!("failed login: user {shown:?} on {surface}");
        let target = if who.is_empty() { shown } else { String::new() };
        self.count_failed(target, who, surface, now());
    }

    /// Counts a failed login in its audit row. A count held back is
    /// written with the next failure, of anyone, [`FAILED_WRITE_SECS`]
    /// later: a burst that stops is still counted whole.
    fn count_failed(&self, target: String, who: &'static str, surface: &'static str, now: i64) {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.retain(|&(_, who, surface), f| {
            let done = f.first <= now - FAILED_ROW_SECS;
            if done || f.written <= now - FAILED_WRITE_SECS {
                self.write_failed(who, surface, f, now);
            }
            !done
        });
        if failed.len() >= 1024 {
            for ((_, who, surface), f) in failed.iter_mut() {
                self.write_failed(who, surface, f, now);
            }
            failed.clear();
        }
        let key = (target, who, surface);
        if let Some(f) = failed.get_mut(&key) {
            f.n += 1;
            if f.n <= 10 || f.written <= now - FAILED_WRITE_SECS {
                self.write_failed(who, surface, f, now);
            }
            return;
        }
        let id = self.db.write(|tx| {
            tx.execute(
                "INSERT INTO audit (time, actor, action, target, detail) VALUES (?1, 'anonymous', 'login failed', ?2, ?3)",
                params![now, key.0, failed_detail(who, surface, 1)],
            )?;
            Ok::<_, rusqlite::Error>(tx.last_insert_rowid())
        });
        if let Ok(id) = id {
            failed.insert(
                key,
                Failures {
                    id,
                    first: now,
                    n: 1,
                    shown: 1,
                    written: now,
                },
            );
        }
    }

    /// Writes the count of `f` to its audit row, when the row says less.
    fn write_failed(&self, who: &str, surface: &str, f: &mut Failures, now: i64) {
        if f.shown == f.n {
            return;
        }
        let detail = failed_detail(who, surface, f.n);
        let _ = self.db.write(|tx| {
            tx.execute(
                "UPDATE audit SET detail = ?1 WHERE id = ?2",
                params![detail, f.id],
            )
        });
        (f.shown, f.written) = (f.n, now);
    }

    /// A token given as a password, with the user name its client sent.
    fn token_login(&self, name: &str, password: &str) -> Option<Caller> {
        let c = self.token(password)?;
        // "__token__" as PyPI clients (twine, uv) send it; a publisher
        // token by its name (trust:<rule>) too.
        if name.is_empty() || name == c.name() || name == "token" || name == "__token__" {
            return Some(c);
        }
        None
    }

    /// The caller a token stands for.
    pub fn token(&self, token: &str) -> Option<Caller> {
        let h = sha(token);
        let t = now();
        type Row = (User, String, bool, Option<i64>, i64, Option<i64>);
        let found: Option<Row> = self
            .db
            .read(|c| {
                c.query_row(
                    "SELECT u.id, u.name, u.admin, u.disabled, u.created, t.repos, t.write, t.expires, t.id, t.used
                     FROM tokens t JOIN users u ON u.id = t.user WHERE t.hash = ?1",
                    [&h],
                    |r| Ok((row_user(r)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
                )
                .optional()
            })
            .ok()?;
        let Some((user, repos, write, expires, id, used)) = found else {
            return self.publisher_token(&h, t);
        };
        if user.disabled || expires.is_some_and(|e| e <= t) {
            return None;
        }
        // "Last used" to the minute: no write on every request apt makes.
        if used.is_none_or(|u| u < t - 60) {
            let _ = self.db.write(|tx| {
                tx.execute("UPDATE tokens SET used = ?1 WHERE id = ?2", params![t, id])
            });
        }
        let exchanged = self
            .db
            .read(|c| {
                c.query_row(
                    "SELECT name LIKE 'trust:%' FROM tokens WHERE id = ?1",
                    [id],
                    |r| r.get::<_, bool>(0),
                )
            })
            .unwrap_or(false);
        Some(Caller {
            user: Some(user),
            scope: Some(Scope {
                repos: serde_json::from_str(&repos).unwrap_or_default(),
                write,
                exchanged,
                publisher: None,
                expires,
            }),
        })
    }

    /// A publisher token: what its rule gives now (a rule removed or
    /// changed takes its tokens' rights with it).
    fn publisher_token(&self, hash: &[u8], t: i64) -> Option<Caller> {
        let (rule, repos, write, expires): (String, String, bool, i64) = self
            .db
            .read(|c| {
                c.query_row(
                    "SELECT r.name, r.repos, r.write, p.expires FROM publish_tokens p
                     JOIN trust_rules r ON r.id = p.rule
                     WHERE p.hash = ?1 AND p.expires > ?2 AND r.user IS NULL",
                    params![hash, t],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
            })
            .ok()??;
        Some(Caller {
            user: None,
            scope: Some(Scope {
                repos: serde_json::from_str(&repos).unwrap_or_default(),
                write,
                exchanged: true,
                publisher: Some(format!("trust:{rule}")),
                expires: Some(expires),
            }),
        })
    }

    /// Makes a publisher token for a rule without a user; returns it
    /// (shown once).
    pub fn create_publisher_token(&self, rule_id: i64, expires: i64) -> Result<String> {
        let token = format!("{TOKEN_PREFIX}{}", random_hex(20));
        let now = now();
        self.db.write(|tx| {
            tx.execute("DELETE FROM publish_tokens WHERE expires < ?1", [now])?;
            tx.execute(
                "INSERT INTO publish_tokens (hash, rule, expires, created) VALUES (?1, ?2, ?3, ?4)",
                params![sha(&token), rule_id, expires, now],
            )
        })?;
        Ok(token)
    }

    // ---- tokens ----

    /// Makes a token; returns it (shown once) and its record.
    pub fn create_token(
        &self,
        user: &str,
        name: &str,
        repos: &[String],
        write: bool,
        expires: Option<i64>,
    ) -> Result<(String, TokenInfo)> {
        let u = self.user(user)?;
        if name.trim().is_empty() {
            return Err(Error::Invalid("a token needs a name".into()));
        }
        let token = format!("{TOKEN_PREFIX}{}", random_hex(20));
        let id = self.db.write(|tx| {
            tx.execute(
                "INSERT INTO tokens (user, name, hash, repos, write, expires, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    u.id,
                    name.trim(),
                    sha(&token),
                    serde_json::to_string(repos).expect("list"),
                    write,
                    expires,
                    now()
                ],
            )?;
            Ok::<_, rusqlite::Error>(tx.last_insert_rowid())
        })?;
        let info = self
            .tokens(Some(user))?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| Error::Invalid("token vanished".into()))?;
        Ok((token, info))
    }

    /// Tokens of a user (or everyone's).
    pub fn tokens(&self, user: Option<&str>) -> Result<Vec<TokenInfo>> {
        Ok(self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT t.id, u.name, t.name, t.repos, t.write, t.expires, t.created, t.used
                 FROM tokens t JOIN users u ON u.id = t.user
                 WHERE ?1 IS NULL OR u.name = ?1 ORDER BY u.name, t.created",
            )?;
            let v = st
                .query_map([user], |r| {
                    let repos: String = r.get(3)?;
                    Ok(TokenInfo {
                        id: r.get(0)?,
                        user: r.get(1)?,
                        name: r.get(2)?,
                        repos: serde_json::from_str(&repos).unwrap_or_default(),
                        write: r.get(4)?,
                        expires: r.get(5)?,
                        created: r.get(6)?,
                        used: r.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    /// Deletes a token; `user`, if given, must own it.
    pub fn delete_token(&self, id: i64, user: Option<&str>) -> Result<()> {
        let n = self.db.write(|tx| {
            tx.execute(
                "DELETE FROM tokens WHERE id = ?1 AND (?2 IS NULL OR user = (SELECT id FROM users WHERE name = ?2))",
                params![id, user],
            )
        })?;
        if n == 0 {
            return Err(Error::NotFound(format!("no token {id}")));
        }
        Ok(())
    }

    // ---- sessions ----

    /// A console session: (cookie value, CSRF token).
    pub fn create_session(&self, user: &User) -> Result<(String, String)> {
        let id = random_hex(24);
        let csrf = random_hex(16);
        let t = now();
        self.db.write(|tx| {
            tx.execute("DELETE FROM sessions WHERE expires < ?1", [t])?;
            tx.execute(
                "INSERT INTO sessions (hash, user, csrf, created, expires) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sha(&id), user.id, csrf, t, t + 12 * 3600],
            )
        })?;
        Ok((id, csrf))
    }

    /// The user and CSRF token of a session.
    pub fn session(&self, id: &str) -> Option<(User, String)> {
        self.db
            .read(|c| {
                c.query_row(
                    "SELECT u.id, u.name, u.admin, u.disabled, u.created, s.csrf
                     FROM sessions s JOIN users u ON u.id = s.user
                     WHERE s.hash = ?1 AND s.expires > ?2 AND NOT u.disabled",
                    params![sha(id), now()],
                    |r| Ok((row_user(r)?, r.get(5)?)),
                )
                .optional()
            })
            .ok()
            .flatten()
    }

    pub fn end_session(&self, id: &str) {
        let _ = self
            .db
            .write(|tx| tx.execute("DELETE FROM sessions WHERE hash = ?1", [sha(id)]));
    }

    // ---- teams ----

    pub fn teams(&self) -> Result<Vec<Team>> {
        Ok(self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT t.name, (SELECT group_concat(u.name, ' ') FROM team_members m JOIN users u ON u.id = m.user WHERE m.team = t.id)
                 FROM teams t ORDER BY t.name",
            )?;
            let v = st
                .query_map([], |r| {
                    let m: Option<String> = r.get(1)?;
                    let mut members: Vec<String> =
                        m.unwrap_or_default().split(' ').filter(|s| !s.is_empty()).map(String::from).collect();
                    members.sort();
                    Ok(Team {
                        name: r.get(0)?,
                        members,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    pub fn create_team(&self, name: &str) -> Result<()> {
        check_user_name(name)?;
        self.db
            .write(|tx| {
                tx.execute(
                    "INSERT INTO teams (name, created) VALUES (?1, ?2)",
                    params![name, now()],
                )
            })
            .map_err(|e| unique(e, &format!("team {name} exists")))?;
        Ok(())
    }

    pub fn delete_team(&self, name: &str) -> Result<()> {
        let n = self
            .db
            .write(|tx| tx.execute("DELETE FROM teams WHERE name = ?1", [name]))?;
        if n == 0 {
            return Err(Error::NotFound(format!("no team {name}")));
        }
        Ok(())
    }

    pub fn set_member(&self, team: &str, user: &str, member: bool) -> Result<()> {
        let u = self.user(user)?;
        let t: i64 = self
            .db
            .read(|c| {
                c.query_row("SELECT id FROM teams WHERE name = ?1", [team], |r| r.get(0))
                    .optional()
            })?
            .ok_or_else(|| Error::NotFound(format!("no team {team}")))?;
        self.db.write(|tx| {
            if member {
                tx.execute(
                    "INSERT OR IGNORE INTO team_members (team, user) VALUES (?1, ?2)",
                    params![t, u.id],
                )
            } else {
                tx.execute(
                    "DELETE FROM team_members WHERE team = ?1 AND user = ?2",
                    params![t, u.id],
                )
            }
        })?;
        Ok(())
    }

    // ---- grants ----

    fn subject_ids(&self, subject: &str) -> Result<(Option<i64>, Option<i64>)> {
        let (kind, name) = subject.split_once(':').ok_or_else(|| {
            Error::Invalid(format!("subject {subject:?}: user:<name> or team:<name>"))
        })?;
        match kind {
            "user" => Ok((Some(self.user(name)?.id), None)),
            "team" => {
                let id = self
                    .db
                    .read(|c| {
                        c.query_row("SELECT id FROM teams WHERE name = ?1", [name], |r| r.get(0))
                            .optional()
                    })?
                    .ok_or_else(|| Error::NotFound(format!("no team {name}")))?;
                Ok((None, Some(id)))
            }
            _ => Err(Error::Invalid(format!(
                "subject {subject:?}: user:<name> or team:<name>"
            ))),
        }
    }

    /// Gives `subject` (`user:x`, `team:y`) `role` on `repo` (or `*`).
    pub fn grant(&self, repo: &str, subject: &str, role: Role) -> Result<()> {
        let (u, t) = self.subject_ids(subject)?;
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM grants WHERE repo = ?1 AND user IS ?2 AND team IS ?3",
                params![repo, u, t],
            )?;
            tx.execute(
                "INSERT INTO grants (repo, user, team, role) VALUES (?1, ?2, ?3, ?4)",
                params![repo, u, t, role.as_str()],
            )
        })?;
        Ok(())
    }

    pub fn revoke(&self, repo: &str, subject: &str) -> Result<()> {
        let (u, t) = self.subject_ids(subject)?;
        let n = self.db.write(|tx| {
            tx.execute(
                "DELETE FROM grants WHERE repo = ?1 AND user IS ?2 AND team IS ?3",
                params![repo, u, t],
            )
        })?;
        if n == 0 {
            return Err(Error::NotFound(format!("{subject} has no grant on {repo}")));
        }
        Ok(())
    }

    /// Grants on `repo` (or all, for `None`).
    pub fn grants(&self, repo: Option<&str>) -> Result<Vec<Grant>> {
        Ok(self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT g.repo, CASE WHEN g.user IS NOT NULL THEN 'user:' || u.name ELSE 'team:' || t.name END, g.role
                 FROM grants g LEFT JOIN users u ON u.id = g.user LEFT JOIN teams t ON t.id = g.team
                 WHERE ?1 IS NULL OR g.repo = ?1 ORDER BY g.repo, 2",
            )?;
            let v = st
                .query_map([repo], |r| {
                    let role: String = r.get(2)?;
                    Ok(Grant {
                        repo: r.get(0)?,
                        subject: r.get(1)?,
                        role: Role::parse(&role).unwrap_or(Role::Read),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }

    /// Removes a deleted repository's grants.
    pub fn forget_repo(&self, repo: &str) -> Result<()> {
        self.db
            .write(|tx| tx.execute("DELETE FROM grants WHERE repo = ?1", [repo]))?;
        Ok(())
    }

    /// The role `c` has on `repo` (`public`: anyone may read it).
    pub fn role(&self, c: &Caller, repo: &str, public: bool) -> Option<Role> {
        let floor = public.then_some(Role::Read);
        // A publisher token: its rule's repositories, never more than
        // write; no grant counts (an empty list is none).
        if let Some(s) = c.scope.as_ref().filter(|s| s.publisher.is_some()) {
            let mine = s.repos.iter().any(|r| r == repo);
            let role = mine.then_some(if s.write { Role::Write } else { Role::Read });
            return role.max(floor);
        }
        let Some(u) = &c.user else {
            return floor;
        };
        let mut role = if u.admin {
            Some(Role::Admin)
        } else {
            let best: Option<String> = self
                .db
                .read(|db| {
                    db.query_row(
                        "SELECT role FROM grants g
                         WHERE (g.repo = ?1 OR g.repo = '*')
                           AND (g.user = ?2 OR g.team IN (SELECT team FROM team_members WHERE user = ?2))
                         ORDER BY CASE role WHEN 'admin' THEN 3 WHEN 'write' THEN 2 ELSE 1 END DESC LIMIT 1",
                        params![repo, u.id],
                        |r| r.get(0),
                    )
                    .optional()
                })
                .ok()
                .flatten();
            best.and_then(|r| Role::parse(&r).ok())
        };
        if let Some(s) = &c.scope {
            if !s.repos.is_empty() && !s.repos.iter().any(|r| r == repo) {
                role = None;
            }
            if !s.write {
                role = role.map(|r| r.min(Role::Read));
            }
        }
        role.max(floor)
    }

    // ---- audit ----

    pub fn audit(&self, actor: &str, action: &str, target: &str, detail: &str) {
        let _ = self.db.write(|tx| {
            tx.execute(
                "INSERT INTO audit (time, actor, action, target, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now(), actor, action, target, detail],
            )
        });
    }

    pub fn audit_log(&self, limit: usize) -> Result<Vec<serde_json::Value>> {
        self.audit_where(None, limit)
    }

    /// What was done to `target` (a repository), newest first.
    pub fn audit_for(&self, target: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        self.audit_where(Some(target), limit)
    }

    fn audit_where(&self, target: Option<&str>, limit: usize) -> Result<Vec<serde_json::Value>> {
        Ok(self.db.read(|c| {
            let mut st = c.prepare(
                "SELECT time, actor, action, target, detail FROM audit \
                 WHERE ?2 IS NULL OR target = ?2 ORDER BY id DESC LIMIT ?1",
            )?;
            let v = st
                .query_map(rusqlite::params![limit as i64, target], |r| {
                    Ok(serde_json::json!({
                        "time": r.get::<_, i64>(0)?,
                        "actor": r.get::<_, String>(1)?,
                        "action": r.get::<_, String>(2)?,
                        "target": r.get::<_, String>(3)?,
                        "detail": r.get::<_, String>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(v)
        })?)
    }
}

fn unique(e: rusqlite::Error, msg: &str) -> Error {
    match e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Error::Conflict(msg.to_string())
        }
        e => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> (tempfile::TempDir, Auth) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&d.path().join("t.db")).unwrap());
        crate::schema::migrate(&db).unwrap();
        (d, Auth::new(db))
    }

    #[test]
    fn roles_from_grants_teams_tokens_and_public() {
        let (_d, a) = auth();
        a.create_user("root", "password1", true).unwrap();
        a.create_user("dev", "password2", false).unwrap();
        a.create_user("ops", "password3", false).unwrap();
        a.create_team("ci").unwrap();
        a.set_member("ci", "ops", true).unwrap();
        a.grant("ppa", "user:dev", Role::Write).unwrap();
        a.grant("*", "team:ci", Role::Read).unwrap();
        let dev = a.login("dev", "password2", "api").unwrap();
        let ops = a.login("ops", "password3", "api").unwrap();
        let root = a.login("root", "password1", "api").unwrap();
        assert!(a.login("dev", "wrong-password", "api").is_none());
        assert!(a.login("nobody", "password2", "api").is_none());
        assert_eq!(a.role(&dev, "ppa", false), Some(Role::Write));
        assert_eq!(a.role(&dev, "other", false), None);
        assert_eq!(a.role(&dev, "other", true), Some(Role::Read));
        assert_eq!(a.role(&ops, "anything", false), Some(Role::Read));
        assert_eq!(a.role(&root, "x", false), Some(Role::Admin));
        assert_eq!(a.role(&Caller::anonymous(), "x", false), None);

        // A read-only token for one repository.
        let (tok, info) = a
            .create_token("dev", "ci", &["ppa".into()], false, None)
            .unwrap();
        assert!(!info.write);
        let t = a.login("", &tok, "api").unwrap();
        assert_eq!(a.role(&t, "ppa", false), Some(Role::Read));
        assert_eq!(a.role(&t, "other", true), Some(Role::Read));
        assert_eq!(a.role(&t, "other", false), None);
        assert!(a.login("someone-else", &tok, "api").is_none());
        // An admin's scoped token is no server admin.
        let (rt, _) = a
            .create_token("root", "x", &["ppa".into()], true, None)
            .unwrap();
        assert!(!a.token(&rt).unwrap().is_admin());
        assert!(root.is_admin());
        // Expired tokens do not work.
        let (old, _) = a.create_token("dev", "old", &[], true, Some(1)).unwrap();
        assert!(a.token(&old).is_none());
        a.delete_token(info.id, Some("dev")).unwrap();
        assert!(a.token(&tok).is_none());
    }

    #[test]
    fn last_admin_stays_and_sessions() {
        let (d, a) = auth();
        assert!(a.bootstrap(&d.path().join("pw")).unwrap());
        assert!(!a.bootstrap(&d.path().join("pw")).unwrap());
        let pw = std::fs::read_to_string(d.path().join("pw")).unwrap();
        assert!(a.login("admin", pw.trim(), "api").is_some());
        assert!(a.set_flags("admin", Some(false), None).is_err());
        assert!(a.delete_user("admin").is_err());
        let u = a.user("admin").unwrap();
        let (sid, csrf) = a.create_session(&u).unwrap();
        assert_eq!(a.session(&sid).unwrap().1, csrf);
        a.set_password("admin", "new-password").unwrap();
        assert!(a.session(&sid).is_none(), "a password change ends sessions");
        assert!(a.create_user("x", "short", false).is_err());
        assert!(a.create_user("bad name", "long-enough", false).is_err());
        // It would be read as a token, never as a password.
        assert!(a.create_user("x", "pkgpod_long-enough", false).is_err());
        assert!(a.set_password("admin", "pkgpod_long-enough").is_err());
    }

    #[test]
    fn failed_logins_are_audited_counted() {
        let (_d, a) = auth();
        a.create_user("dev", "password2", false).unwrap();
        for _ in 0..3 {
            assert!(a.login("dev", "wrong-password", "api").is_none());
        }
        assert!(a.login("dev", "wrong-password", "console").is_none());
        assert!(a.login("nobody", "password2", "api").is_none());
        assert!(a.login("someone", "password2", "api").is_none());
        assert!(a.login("dev", "pkgpod_not-a-token", "serving").is_none());
        // Refused without a password check, as fast as they come: the
        // count is written for the first ten, then now and then.
        for _ in 0..19 {
            assert!(a.login("dev", "pkgpod_not-a-token", "serving").is_none());
        }
        assert!(a.login("dev", "password2", "api").is_some());
        let rows = || -> Vec<String> {
            a.audit_log(10)
                .unwrap()
                .iter()
                .filter(|r| r["action"] == "login failed")
                .map(|r| format!("{} {}", r["target"], r["detail"]))
                .collect()
        };
        assert_eq!(
            rows(),
            [
                r#""" "serving, 10 times, tokens""#,
                r#""" "api, 2 times, unknown users""#,
                r#""dev" "console, 1 time""#,
                r#""dev" "api, 3 times""#,
            ]
        );
        // What was held back is written with a later failure.
        a.count_failed("dev".into(), "", "serving", now() + FAILED_WRITE_SECS);
        assert_eq!(
            rows(),
            [
                r#""dev" "serving, 1 time""#,
                r#""" "serving, 20 times, tokens""#,
                r#""" "api, 2 times, unknown users""#,
                r#""dev" "console, 1 time""#,
                r#""dev" "api, 3 times""#,
            ]
        );
    }
}
