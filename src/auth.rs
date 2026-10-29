//! Users, argon2 password hashes, session cookies, per-IP login throttle.

use crate::db;
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const COOKIE: &str = "xmusic_session";
const SESSION_DAYS: i64 = 30;

pub struct User {
    pub id: i64,
    pub name: String,
    pub admin: bool,
}

pub fn hash(password: &str) -> String {
    Argon2::default().hash_password(password.as_bytes()).unwrap().to_string()
}

fn verify(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn user_count(c: &Connection) -> i64 {
    c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0)).unwrap()
}

pub fn create_user(c: &Connection, name: &str, password: &str, role: &str) -> Result<i64, String> {
    if name.trim().is_empty() || password.len() < 8 {
        return Err("name required and password must be at least 8 characters".into());
    }
    c.execute("INSERT INTO users(name,hash,role) VALUES(?1,?2,?3)", params![name.trim(), hash(password), role])
        .map_err(|_| "user already exists".to_string())?;
    Ok(c.last_insert_rowid())
}

/// Returns a new session token for valid credentials.
pub fn login(c: &Connection, name: &str, password: &str) -> Option<String> {
    let (id, phc): (i64, String) = c
        .query_row("SELECT id,hash FROM users WHERE name=?1", [name.trim()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .unwrap()?;
    verify(password, &phc).then(|| new_session(c, id))
}

pub fn new_session(c: &Connection, user_id: i64) -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("os rng");
    let token = hex::encode(b);
    c.execute("DELETE FROM sessions WHERE expires<?1", [db::now()]).unwrap();
    c.execute(
        "INSERT INTO sessions(token_hash,user_id,expires) VALUES(?1,?2,?3)",
        params![token_hash(&token), user_id, db::now() + SESSION_DAYS * 86400],
    )
    .unwrap();
    token
}

pub fn logout(c: &Connection, token: &str) {
    c.execute("DELETE FROM sessions WHERE token_hash=?1", [token_hash(token)]).unwrap();
}

pub fn session_user(c: &Connection, token: &str) -> Option<User> {
    c.query_row(
        "SELECT u.id,u.name,u.role FROM sessions s JOIN users u ON u.id=s.user_id WHERE s.token_hash=?1 AND s.expires>?2",
        params![token_hash(token), db::now()],
        |r| Ok(User { id: r.get(0)?, name: r.get(1)?, admin: r.get::<_, String>(2)? == "admin" }),
    )
    .optional()
    .unwrap()
}

pub fn set_password(c: &Connection, name: &str, password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters".into());
    }
    let n = c.execute("UPDATE users SET hash=?1 WHERE name=?2", params![hash(password), name]).unwrap();
    if n == 0 {
        return Err(format!("no user {name}"));
    }
    c.execute("DELETE FROM sessions WHERE user_id=(SELECT id FROM users WHERE name=?1)", [name]).unwrap();
    Ok(())
}

// 5 failures per IP → locked for a minute
static FAILS: LazyLock<Mutex<HashMap<IpAddr, (u32, Instant)>>> = LazyLock::new(Default::default);

pub fn throttled(ip: IpAddr) -> bool {
    FAILS.lock().unwrap().get(&ip).is_some_and(|(n, t)| *n >= 5 && t.elapsed() < Duration::from_secs(60))
}

pub fn record(ip: IpAddr, ok: bool) {
    let mut f = FAILS.lock().unwrap();
    if ok {
        f.remove(&ip);
        return;
    }
    let e = f.entry(ip).or_insert((0, Instant::now()));
    if e.1.elapsed() > Duration::from_secs(60) {
        *e = (0, Instant::now());
    }
    e.0 += 1;
    e.1 = Instant::now();
}
