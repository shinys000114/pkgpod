// SPDX-License-Identifier: AGPL-3.0-only

//! Trusted publishing: a CI job proves who it is with the OIDC token its
//! CI gives it (GitHub Actions' among them) and gets a short-lived pkgpod
//! token in exchange, as a trust rule allows. No long-lived secret is
//! kept in the CI.
//!
//! A rule names an issuer, the audience the token must be made for, the
//! claims it must carry (`repository`, `ref`, `workflow_ref`,
//! `environment`, …; `*` matches any run of characters), the pkgpod user
//! whose token is given, the repositories it is limited to and how long
//! it lasts. The token can never do more than its user may.
//!
//! The issuer's keys come from its OpenID discovery document (https, or
//! http on the loopback for tests); a token is checked for its
//! signature (RS256 or ES256), issuer, audience, expiry, and is used
//! once.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::{App, Error, Result};

/// GitHub Actions' issuer.
pub const GITHUB: &str = "https://token.actions.githubusercontent.com";
/// The audience a rule expects unless told otherwise.
pub const AUDIENCE: &str = "pkgpod";
/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY: i64 = 60;
/// How long an issuer's keys are used before they are asked for again.
const KEYS_KEPT: Duration = Duration::from_secs(600);
/// How often at most an issuer's keys are asked for again for a token
/// whose key is not among them: else any caller could make the server
/// ask with every token it sends.
const KEYS_ASKED_AGAIN: Duration = Duration::from_secs(60);
/// The most an issuer's discovery document or key set may take.
const MAX_DOC: usize = 1 << 20;

/// A trust rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default = "github")]
    pub issuer: String,
    #[serde(default = "audience")]
    pub audience: String,
    /// Claim -> pattern; all must match.
    pub claims: BTreeMap<String, String>,
    /// The pkgpod user whose token is given; empty: none, a publisher
    /// token (its repositories written to, or read, and nothing else).
    #[serde(default)]
    pub user: String,
    /// What the token is limited to.
    pub repos: Vec<String>,
    #[serde(default = "yes")]
    pub write: bool,
    /// The token's life.
    #[serde(default = "fifteen")]
    pub minutes: i64,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub used: Option<i64>,
}

fn github() -> String {
    GITHUB.into()
}
fn audience() -> String {
    AUDIENCE.into()
}
fn yes() -> bool {
    true
}
fn fifteen() -> i64 {
    15
}

/// Whether `pat` (`*`: any run of characters) matches all of `s`.
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for p in &parts[1..parts.len() - 1] {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => return false,
        }
    }
    true
}

/// A claim as text (strings as they are, numbers and booleans written).
fn claim_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl Rule {
    /// Whether the token's claims satisfy every claim of the rule.
    pub fn matches(&self, claims: &Value) -> bool {
        self.claims.iter().all(|(k, pat)| {
            claims
                .get(k)
                .and_then(claim_text)
                .is_some_and(|v| glob(pat, &v))
        })
    }

    /// Whether `o` says what this says (when made and used aside).
    fn says_as(&self, o: &Rule) -> bool {
        self.name == o.name
            && self.issuer == o.issuer
            && self.audience == o.audience
            && self.claims == o.claims
            && self.user == o.user
            && self.repos == o.repos
            && self.write == o.write
            && self.minutes == o.minutes
    }
}

/// An issuer's address: https, or http on the loopback (tests, a local
/// CI); never with a user or password in it.
fn check_issuer(url: &str) -> Result<()> {
    let ok = reqwest::Url::parse(url).is_ok_and(|u| {
        let host = u.host_str().unwrap_or("");
        let loopback = host == "localhost"
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|a| a.is_loopback());
        u.username().is_empty()
            && u.password().is_none()
            && !host.is_empty()
            && match u.scheme() {
                "https" => true,
                "http" => loopback,
                _ => false,
            }
    });
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "issuer {url:?}: an https address (http only on the loopback)"
        )))
    }
}

// ---- rules ----

/// Rules as [`rule_row`] reads them, with their ids and user ids after.
const RULES: &str =
    "SELECT r.name, r.issuer, r.audience, r.claims, COALESCE(u.name, ''), r.repos, r.write,
        r.minutes, r.created, r.used, r.id, r.user
     FROM trust_rules r LEFT JOIN users u ON u.id = r.user";

fn rule_row(r: &rusqlite::Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        name: r.get(0)?,
        issuer: r.get(1)?,
        audience: r.get(2)?,
        claims: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        user: r.get(4)?,
        repos: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        write: r.get(6)?,
        minutes: r.get(7)?,
        created: r.get(8)?,
        used: r.get(9)?,
    })
}

pub fn rules(app: &App) -> Result<Vec<Rule>> {
    Ok(app.db.read(|c| {
        let mut st = c.prepare(&format!("{RULES} ORDER BY r.name"))?;
        st.query_map([], rule_row)?
            .collect::<rusqlite::Result<Vec<_>>>()
    })?)
}

/// Adds a rule, or replaces the one of the same name.
pub fn put_rule(app: &App, mut r: Rule) -> Result<Rule> {
    r.name = r.name.trim().to_string();
    if r.name.is_empty()
        || !r
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Invalid(
            "a rule's name: letters, digits, '-', '_', '.'".into(),
        ));
    }
    r.issuer = r.issuer.trim().trim_end_matches('/').to_string();
    check_issuer(&r.issuer)?;
    if r.audience.trim().is_empty() {
        return Err(Error::Invalid("a rule needs an audience".into()));
    }
    r.claims
        .retain(|k, v| !k.trim().is_empty() && !v.trim().is_empty());
    // Who it trusts is said by at least one claim that is not a bare *.
    if !r.claims.values().any(|v| v.chars().any(|c| c != '*')) {
        return Err(Error::Invalid(
            "a rule needs a claim that names who it trusts (repository=owner/name, say)".into(),
        ));
    }
    if r.issuer == GITHUB
        && !["repository", "repository_id", "sub"]
            .iter()
            .any(|k| r.claims.get(*k).is_some_and(|v| !v.contains('*')))
    {
        return Err(Error::Invalid(
            "a GitHub rule names its repository (repository=owner/name, no *): \
             any repository could otherwise ask"
                .into(),
        ));
    }
    if r.repos.is_empty() {
        return Err(Error::Invalid(
            "a rule names the repositories its tokens are for".into(),
        ));
    }
    // Names it already had stay, a repository deleted since included.
    let had: Vec<String> = app
        .db
        .read(|c| {
            c.query_row(
                "SELECT repos FROM trust_rules WHERE name = ?1",
                [&r.name],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for repo in r.repos.iter().filter(|x| !had.contains(x)) {
        app.engine
            .repo(repo)
            .map_err(|_| Error::NotFound(format!("no repository {repo:?}")))?;
    }
    if !(1..=60).contains(&r.minutes) {
        return Err(Error::Invalid("a token lasts 1 to 60 minutes".into()));
    }
    r.user = r.user.trim().to_string();
    let uid = if r.user.is_empty() {
        None
    } else {
        let u = app.auth.user(&r.user)?;
        if u.disabled {
            return Err(Error::Invalid(format!("user {} is disabled", r.user)));
        }
        Some(u.id)
    };
    let now = pkgpod_base::now();
    app.db.write(|tx| {
        tx.execute(
            "INSERT INTO trust_rules (name, issuer, audience, claims, user, repos, write, minutes, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET issuer = ?2, audience = ?3, claims = ?4, user = ?5,
               repos = ?6, write = ?7, minutes = ?8",
            params![
                r.name,
                r.issuer,
                r.audience.trim(),
                serde_json::to_string(&r.claims).expect("map"),
                uid,
                serde_json::to_string(&r.repos).expect("list"),
                r.write,
                r.minutes,
                now
            ],
        )
    })?;
    // Tokens it gave under what it said before go: a rule saved again
    // gives new ones under what it says now.
    app.db.write(|tx| {
        tx.execute(
            "DELETE FROM publish_tokens WHERE rule = (SELECT id FROM trust_rules WHERE name = ?1)",
            [&r.name],
        )?;
        drop_user_tokens(tx, &r.name)
    })?;
    rules(app)?
        .into_iter()
        .find(|x| x.name == r.name)
        .ok_or_else(|| Error::Invalid("rule vanished".into()))
}

/// A repository deleted: rules without a user name it no more (one
/// made again by that name gets no publisher), and those left with no
/// repository go.
pub fn forget_repo(app: &App, repo: &str) -> Result<()> {
    for r in rules(app)?
        .into_iter()
        .filter(|r| r.user.is_empty() && r.repos.iter().any(|x| x == repo))
    {
        let left: Vec<&String> = r.repos.iter().filter(|x| *x != repo).collect();
        app.db.write(|tx| {
            if left.is_empty() {
                tx.execute("DELETE FROM trust_rules WHERE name = ?1", [&r.name])
            } else {
                tx.execute(
                    "UPDATE trust_rules SET repos = ?1 WHERE name = ?2",
                    params![serde_json::to_string(&left).expect("list"), r.name],
                )
            }
        })?;
    }
    Ok(())
}

pub fn remove_rule(app: &App, name: &str) -> Result<()> {
    let n = app.db.write(|tx| {
        let n = tx.execute("DELETE FROM trust_rules WHERE name = ?1", [name])?;
        if n > 0 {
            drop_user_tokens(tx, name)?;
        }
        Ok::<_, rusqlite::Error>(n)
    })?;
    if n == 0 {
        return Err(Error::NotFound(format!("no trust rule {name:?}")));
    }
    Ok(())
}

/// The name of the tokens of a user that `rule` gives, before the CI's
/// claims (no other token may begin with `trust:`).
fn token_name(rule: &str) -> String {
    format!("trust:{rule}")
}

/// Deletes the tokens of a user that `rule` gave (a publisher token is
/// checked against its rule when used; these are not).
fn drop_user_tokens(tx: &rusqlite::Transaction, rule: &str) -> rusqlite::Result<usize> {
    let name = token_name(rule);
    tx.execute(
        "DELETE FROM tokens WHERE name = ?1 OR substr(name, 1, length(?2)) = ?2",
        params![name, format!("{name} ")],
    )
}

// ---- tokens ----

/// A JSON Web Key, as far as RS256 and ES256 need it.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
    #[serde(default)]
    crv: String,
    #[serde(default)]
    x: String,
    #[serde(default)]
    y: String,
}

/// Issuers' keys and the tokens already exchanged.
#[derive(Default)]
pub struct Verifier {
    keys: Mutex<HashMap<String, (Instant, Vec<Jwk>)>>,
    /// When an issuer's keys were last asked for again for an unknown key.
    asked: Mutex<HashMap<String, Instant>>,
    /// When asking an issuer for its keys last failed, and why.
    failed: Mutex<HashMap<String, (Instant, String)>>,
    /// One asking at a time per issuer: who comes meanwhile without
    /// fresh keys waits, then takes what it got.
    asking: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    seen: Mutex<HashMap<String, i64>>,
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("pkgpod/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("an HTTP client")
    })
}

/// A JSON document of at most [`MAX_DOC`] bytes.
async fn fetch_json(url: &str) -> std::result::Result<Value, String> {
    let mut r = client()
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&c);
        if body.len() > MAX_DOC {
            return Err(format!("{url}: more than {MAX_DOC} bytes"));
        }
    }
    serde_json::from_slice(&body).map_err(|e| format!("{url}: {e}"))
}

/// An issuer's keys, from the key set its discovery document names.
async fn fetch_keys(issuer: &str) -> std::result::Result<Vec<Jwk>, String> {
    let doc = fetch_json(&format!("{issuer}/.well-known/openid-configuration")).await?;
    let uri = doc["jwks_uri"]
        .as_str()
        .ok_or("no jwks_uri in its discovery document")?;
    check_issuer(uri).map_err(|e| e.to_string())?;
    let set = fetch_json(uri).await?;
    Ok(serde_json::from_value(set["keys"].clone()).unwrap_or_default())
}

fn b64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|_| Error::Forbidden("the OIDC token is not well formed".into()))
}

impl Verifier {
    /// The issuer's keys (from its discovery document), fetched again
    /// when old or when asked to (that at most every
    /// [`KEYS_ASKED_AGAIN`]); after a failure, not asked for again
    /// within that either.
    async fn keys(&self, issuer: &str, again: bool) -> Result<Vec<Jwk>> {
        let fail = |e: String| Error::Forbidden(format!("the issuer's keys: {e}"));
        // Keys kept fresh need no wait behind an asking (one any caller
        // can bring about with a key the issuer does not have).
        {
            let kept = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, k)) = kept.get(issuer)
                && !again
                && at.elapsed() < KEYS_KEPT
            {
                return Ok(k.clone());
            }
        }
        // Other tokens that come while the issuer is asked wait for its
        // answer (or its timeout) and take that, instead of asking each.
        let one = self
            .asking
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(issuer.to_string())
            .or_default()
            .clone();
        let _one = one.lock().await;
        {
            let kept = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            let kept = kept.get(issuer);
            let mut asked = self.asked.lock().unwrap_or_else(|e| e.into_inner());
            let lately = asked
                .get(issuer)
                .is_some_and(|t| t.elapsed() < KEYS_ASKED_AGAIN);
            match kept {
                Some((at, k)) if !again && at.elapsed() < KEYS_KEPT => return Ok(k.clone()),
                Some((_, k)) if again && lately => return Ok(k.clone()),
                _ => {}
            }
            // An issuer that failed (down, or a rule's address wrong) is
            // not asked again at once either: else any caller could make
            // the server ask it with every token it sends.
            let failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((t, e)) = failed.get(issuer)
                && t.elapsed() < KEYS_ASKED_AGAIN
            {
                return Err(fail(e.clone()));
            }
            if again {
                asked.insert(issuer.to_string(), Instant::now());
            }
        }
        let keys = match fetch_keys(issuer).await {
            Ok(k) => k,
            Err(e) => {
                self.failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(issuer.to_string(), (Instant::now(), e.clone()));
                return Err(fail(e));
            }
        };
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(issuer);
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(issuer.to_string(), (Instant::now(), keys.clone()));
        Ok(keys)
    }

    /// The claims of a token whose signature, issuer and times hold;
    /// `issuers`: the ones some rule names.
    pub async fn verify(&self, jwt: &str, issuers: &[String]) -> Result<Value> {
        let bad = || Error::Forbidden("the OIDC token is not well formed".into());
        let mut parts = jwt.trim().split('.');
        let (Some(h), Some(p), Some(sig), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let header: Value = serde_json::from_slice(&b64(h)?).map_err(|_| bad())?;
        let claims: Value = serde_json::from_slice(&b64(p)?).map_err(|_| bad())?;
        let sig = b64(sig)?;
        let iss = claims["iss"]
            .as_str()
            .ok_or_else(bad)?
            .trim_end_matches('/');
        if !issuers.iter().any(|i| i == iss) {
            return Err(Error::Forbidden(format!(
                "no trust rule names the issuer {iss}"
            )));
        }
        let alg = header["alg"].as_str().unwrap_or("");
        let kid = header["kid"].as_str().unwrap_or("");
        let input = format!("{h}.{p}");
        let mut ok = false;
        for again in [false, true] {
            let keys = self.keys(iss, again).await?;
            let mut found = keys
                .iter()
                .filter(|k| kid.is_empty() || k.kid == kid)
                .peekable();
            if found.peek().is_none() {
                continue;
            }
            ok = found.any(|k| check(alg, k, input.as_bytes(), &sig));
            break;
        }
        if !ok {
            return Err(Error::Forbidden(
                "the OIDC token's signature does not verify with its issuer's keys".into(),
            ));
        }
        let now = pkgpod_base::now();
        let exp = claims["exp"].as_i64().ok_or_else(bad)?;
        if exp + LEEWAY < now {
            return Err(Error::Forbidden("the OIDC token has expired".into()));
        }
        if claims["nbf"].as_i64().is_some_and(|n| n - LEEWAY > now) {
            return Err(Error::Forbidden("the OIDC token is not valid yet".into()));
        }
        Ok(claims)
    }

    /// Whether a token (by its issuer and what it signs, or its jti) was
    /// exchanged before; remembers it until it expires.
    fn used_before(&self, id: &str, exp: i64) -> bool {
        let now = pkgpod_base::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, e| *e + LEEWAY >= now);
        seen.insert(id.to_string(), exp).is_some()
    }
}

/// Whether `sig` is `alg`'s signature of `input` by key `k`.
fn check(alg: &str, k: &Jwk, input: &[u8], sig: &[u8]) -> bool {
    let hash = sha2::Sha256::digest(input);
    let d = |s: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .ok()
    };
    match (alg, k.kty.as_str()) {
        ("RS256", "RSA") => {
            let (Some(n), Some(e)) = (d(&k.n), d(&k.e)) else {
                return false;
            };
            let Ok(key) = rsa::RsaPublicKey::new(
                rsa::BigUint::from_bytes_be(&n),
                rsa::BigUint::from_bytes_be(&e),
            ) else {
                return false;
            };
            let scheme = rsa::Pkcs1v15Sign {
                hash_len: Some(32),
                prefix: crate::auth::rsa_key::SHA256_PREFIX.into(),
            };
            key.verify(scheme, &hash, sig).is_ok()
        }
        ("ES256", "EC") if k.crv == "P-256" => {
            use p256::ecdsa::signature::hazmat::PrehashVerifier;
            let (Some(x), Some(y)) = (d(&k.x), d(&k.y)) else {
                return false;
            };
            if x.len() != 32 || y.len() != 32 {
                return false;
            }
            let point = p256::EncodedPoint::from_affine_coordinates(
                p256::FieldBytes::from_slice(&x),
                p256::FieldBytes::from_slice(&y),
                false,
            );
            let (Ok(key), Ok(sig)) = (
                p256::ecdsa::VerifyingKey::from_encoded_point(&point),
                p256::ecdsa::Signature::from_slice(sig),
            ) else {
                return false;
            };
            key.verify_prehash(&hash, &sig).is_ok()
        }
        _ => false,
    }
}

/// Exchanges a CI's OIDC token for a pkgpod token, as a trust rule
/// allows (`rule`: the one to use when several match).
pub async fn exchange(app: &Arc<App>, jwt: &str, rule: Option<&str>) -> Result<Value> {
    let all = {
        let app2 = app.clone();
        crate::api::blocking(app, move |_| rules(&app2)).await?
    };
    let issuers: Vec<String> = all.iter().map(|r| r.issuer.clone()).collect();
    let claims = app.oidc.verify(jwt, &issuers).await?;
    let iss = claims["iss"].as_str().unwrap_or("").trim_end_matches('/');
    let auds: Vec<String> = match &claims["aud"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let found: Vec<&Rule> = all
        .iter()
        .filter(|r| r.issuer == iss && auds.contains(&r.audience) && r.matches(&claims))
        .filter(|r| rule.is_none_or(|n| r.name == n))
        .collect();
    let who = ["repository", "ref", "workflow_ref", "environment", "sub"]
        .iter()
        .filter_map(|k| {
            claims
                .get(*k)
                .and_then(claim_text)
                .map(|v| format!("{k}={v}"))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let r = match found.as_slice() {
        [] => {
            return Err(Error::Forbidden(format!(
                "no trust rule matches this token (audience {}; {who})",
                auds.join(", ")
            )));
        }
        [r] => (*r).clone(),
        many => {
            return Err(Error::Invalid(format!(
                "several trust rules match ({}): name one with rule",
                many.iter()
                    .map(|r| r.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    let exp = claims["exp"].as_i64().unwrap_or(0);
    // By what is signed, not by the signature (it can be written in more
    // than one way for the same token), and by its jti if it has one (the
    // issuer's word that it is one token, whatever else differs).
    let signed = jwt.trim().rsplit_once('.').map_or("", |x| x.0);
    let id = pkgpod_base::digest::hex(&sha2::Sha256::digest(signed.as_bytes()));
    let mut ids = vec![format!("{iss} {id}")];
    if let Some(j) = claims["jti"].as_str() {
        ids.push(format!("{iss} jti {j}"));
    }
    // Every id is remembered, whichever was seen.
    let seen: Vec<bool> = ids.iter().map(|i| app.oidc.used_before(i, exp)).collect();
    if seen.contains(&true) {
        return Err(Error::Forbidden(
            "this OIDC token was exchanged already: ask the CI for a new one".into(),
        ));
    }
    let (r2, who2) = (r.clone(), who.clone());
    let (token, info) = crate::api::blocking(app, move |app| give(app, &r2, &who2)).await?;
    let actor = if r.user.is_empty() {
        format!("trust:{}", r.name)
    } else {
        r.user.clone()
    };
    app.auth.audit(&actor, "trust token", &r.name, &who);
    Ok(json!({
        "token": token,
        "expires": info,
        "user": (!r.user.is_empty()).then_some(&r.user),
        "repos": r.repos,
        "write": r.write,
        "rule": r.name,
    }))
}

/// Makes the token rule `r` gives for a CI's claims (`who`); returns it
/// and when it expires.
fn give(app: &App, r: &Rule, who: &str) -> Result<(String, Option<i64>)> {
    let now = pkgpod_base::now();
    let expires = now + r.minutes * 60;
    // The claims cut to fit, never the rule's name its tokens are
    // found by.
    let mut name = format!("{} ", token_name(&r.name));
    let mut end = who.len().min(200usize.saturating_sub(name.len()));
    while !who.is_char_boundary(end) {
        end -= 1;
    }
    name.push_str(&who[..end]);
    let token = app.db.write(|tx| {
        // Old exchanged tokens go: one is made for every CI run.
        tx.execute(
            "DELETE FROM tokens WHERE name LIKE 'trust:%' AND expires IS NOT NULL AND expires < ?1",
            [now - 86400],
        )?;
        // The rule read again where its token is made: one changed or
        // removed while the CI's token was checked gives none, else the
        // token would outlive those the change took away (a change after
        // this transaction takes it away too).
        let found = tx
            .query_row(&format!("{RULES} WHERE r.name = ?1"), [&r.name], |x| {
                Ok((rule_row(x)?, x.get::<_, i64>(10)?, x.get(11)?))
            })
            .optional()?;
        let Some((_, id, user)) = found.filter(|(now_r, ..)| now_r.says_as(r)) else {
            return Err(Error::Forbidden(format!(
                "the trust rule {} changed meanwhile: ask again",
                r.name
            )));
        };
        tx.execute(
            "UPDATE trust_rules SET used = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(match user {
            // A publisher token: checked against its rule when used.
            None => crate::auth::insert_publisher_token(tx, id, expires)?,
            Some(u) => crate::auth::insert_token(tx, u, &name, &r.repos, r.write, Some(expires))?.0,
        })
    })?;
    Ok((token, Some(expires)))
}

/// Whether a trust rule of this name exists (for the console).
pub fn has_rule(app: &App, name: &str) -> bool {
    app.db
        .read(|c| {
            c.query_row("SELECT 1 FROM trust_rules WHERE name = ?1", [name], |_| {
                Ok(())
            })
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
}

/// A fake issuer for tests: a fixed P-256 key, its discovery document
/// and keys, and tokens it signs.
#[cfg(any(test, feature = "testutil"))]
pub mod testutil {
    use base64::Engine as _;
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use serde_json::{Value, json};
    use sha2::Digest as _;

    pub struct Issuer {
        key: p256::ecdsa::SigningKey,
        pub url: String,
    }

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    impl Issuer {
        pub fn new(url: &str) -> Issuer {
            let key = p256::ecdsa::SigningKey::from_slice(&[7u8; 32]).expect("a key");
            Issuer {
                key,
                url: url.trim_end_matches('/').to_string(),
            }
        }

        pub fn discovery(&self) -> Value {
            json!({"issuer": self.url, "jwks_uri": format!("{}/keys", self.url)})
        }

        pub fn jwks(&self) -> Value {
            let p = self.key.verifying_key().to_encoded_point(false);
            json!({"keys": [{
                "kty": "EC", "crv": "P-256", "kid": "test", "alg": "ES256", "use": "sig",
                "x": b64(p.x().expect("x")), "y": b64(p.y().expect("y")),
            }]})
        }

        /// A token with these claims (iss, iat and exp added when absent).
        pub fn token(&self, mut claims: Value) -> String {
            let now = pkgpod_base::now();
            let m = claims.as_object_mut().expect("an object");
            m.entry("iss").or_insert(json!(self.url));
            m.entry("iat").or_insert(json!(now));
            m.entry("exp").or_insert(json!(now + 300));
            let h = b64(br#"{"alg":"ES256","kid":"test","typ":"JWT"}"#);
            let p = b64(claims.to_string().as_bytes());
            let hash = sha2::Sha256::digest(format!("{h}.{p}").as_bytes());
            let sig: p256::ecdsa::Signature = self.key.sign_prehash(&hash).expect("signs");
            format!("{h}.{p}.{}", b64(&sig.to_bytes()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("refs/tags/v*", "refs/tags/v1.2"));
        assert!(!glob("refs/tags/v*", "refs/heads/v1"));
        assert!(glob(
            "org/repo/.github/workflows/release.yml@*",
            "org/repo/.github/workflows/release.yml@refs/tags/v1"
        ));
        assert!(glob("a*b*c", "aXXbYYc") && !glob("a*b*c", "aXXcYYb"));
        assert!(glob("exact", "exact") && !glob("exact", "exactly"));
        assert!(glob("*", "") && !glob("ab*ba", "aba"));
    }

    #[test]
    fn rules_match_claims() {
        let r = Rule {
            name: "r".into(),
            issuer: GITHUB.into(),
            audience: AUDIENCE.into(),
            claims: [
                ("repository".to_string(), "org/repo".to_string()),
                ("ref".to_string(), "refs/tags/v*".to_string()),
                ("repository_id".to_string(), "42".to_string()),
            ]
            .into(),
            user: "ci".into(),
            repos: vec!["ppa".into()],
            write: true,
            minutes: 15,
            created: 0,
            used: None,
        };
        let ok = json!({"repository": "org/repo", "ref": "refs/tags/v1.0", "repository_id": 42});
        assert!(r.matches(&ok));
        for bad in [
            json!({"repository": "org/repo2", "ref": "refs/tags/v1.0", "repository_id": 42}),
            json!({"repository": "org/repo", "ref": "refs/heads/master", "repository_id": 42}),
            json!({"repository": "org/repo", "ref": "refs/tags/v1.0", "repository_id": 43}),
            json!({"repository": "org/repo", "ref": "refs/tags/v1.0"}),
        ] {
            assert!(!r.matches(&bad), "{bad}");
        }
    }

    #[test]
    fn issuers_are_https_or_loopback() {
        assert!(check_issuer(GITHUB).is_ok());
        assert!(check_issuer("http://127.0.0.1:8080").is_ok());
        assert!(check_issuer("http://localhost/x").is_ok());
        assert!(check_issuer("http://example.org").is_err());
        assert!(check_issuer("http://127.0.0.1.example.org").is_err());
        assert!(check_issuer("ftp://x").is_err());
        assert!(check_issuer("http://[::1]:8080").is_ok());
        // What reads as the loopback to a prefix check, but is not.
        assert!(check_issuer("http://localhost:80@example.invalid").is_err());
        assert!(check_issuer("http://127.0.0.1@example.invalid/").is_err());
        assert!(check_issuer("http://[::ffff:1.2.3.4]").is_err());
        assert!(check_issuer("http://[2001:db8::1]/").is_err());
        assert!(check_issuer("https://user:pass@token.example.org").is_err());
        assert!(check_issuer("https://").is_err());
    }

    /// A token naming a key its issuer does not have makes the server
    /// ask for the keys again, but not for every such token.
    #[tokio::test]
    async fn unknown_keys_are_asked_for_seldom() {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let iss = testutil::Issuer::new(&url);
        let (doc, jwks) = (iss.discovery().to_string(), iss.jwks().to_string());
        let asked = Arc::new(AtomicUsize::new(0));
        let a2 = asked.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                let _ = r.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                a2.fetch_add(1, Ordering::SeqCst);
                let body = if line.contains("openid-configuration") {
                    &doc
                } else {
                    &jwks
                };
                let _ = write!(
                    conn,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let v = Verifier::default();
        let t = iss.token(json!({"aud": "pkgpod"}));
        let rest = t.split_once('.').unwrap().1;
        let other = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"ES256","kid":"other","typ":"JWT"}"#);
        let issuers = [url.clone()];
        for _ in 0..5 {
            assert!(
                v.verify(&format!("{other}.{rest}"), &issuers)
                    .await
                    .is_err()
            );
        }
        // The two documents once, and once again for the unknown key.
        assert_eq!(asked.load(Ordering::SeqCst), 4);
        assert!(v.verify(&t, &issuers).await.is_ok());
        assert_eq!(asked.load(Ordering::SeqCst), 4);
    }

    /// A rule changed or removed while a CI's token was checked gives
    /// no token: it would outlive the ones the change took away.
    #[test]
    fn a_rule_changed_meanwhile_gives_no_token() {
        let d = tempfile::tempdir().unwrap();
        let app = App::open(crate::config::Config {
            data_dir: d.path().join("data"),
            ..Default::default()
        })
        .unwrap();
        app.engine
            .create_repo(
                pkgpod_engine::NewRepo {
                    name: "r1".into(),
                    format: "files".into(),
                    kind: pkgpod_engine::Kind::Hosted,
                    storage: None,
                    settings: json!({}),
                },
                "test",
            )
            .unwrap();
        app.auth.create_user("ci", "password1", false).unwrap();
        let rule = Rule {
            name: "rel".into(),
            issuer: "http://127.0.0.1:1".into(),
            audience: AUDIENCE.into(),
            claims: [("repository".to_string(), "me/x".to_string())].into(),
            user: "ci".into(),
            repos: vec!["r1".into()],
            write: true,
            minutes: 30,
            created: 0,
            used: None,
        };
        let refused = |r: &Rule| match give(&app, r, "repository=me/x") {
            Err(Error::Forbidden(_)) => true,
            Err(e) => panic!("{e}"),
            Ok(_) => false,
        };
        let tokens = || app.auth.tokens(Some("ci")).unwrap().len();
        // Removed.
        let seen = put_rule(&app, rule.clone()).unwrap();
        remove_rule(&app, "rel").unwrap();
        assert!(refused(&seen));
        assert_eq!(tokens(), 0);
        // Made to read only, or to trust another repository.
        let seen = put_rule(&app, rule.clone()).unwrap();
        put_rule(
            &app,
            Rule {
                write: false,
                ..rule.clone()
            },
        )
        .unwrap();
        assert!(refused(&seen));
        let seen = put_rule(&app, rule.clone()).unwrap();
        let mut other = rule.clone();
        other.claims.insert("repository".into(), "me/y".into());
        put_rule(&app, other).unwrap();
        assert!(refused(&seen));
        assert_eq!(tokens(), 0);
        // As it was read: given.
        let seen = put_rule(&app, rule.clone()).unwrap();
        let (t, _) = give(&app, &seen, "repository=me/x").unwrap();
        assert!(app.auth.token(&t).is_some());
        // A publisher's rule the same.
        let publ = Rule {
            name: "pub".into(),
            user: String::new(),
            ..rule.clone()
        };
        let seen = put_rule(&app, publ.clone()).unwrap();
        put_rule(
            &app,
            Rule {
                write: false,
                ..publ.clone()
            },
        )
        .unwrap();
        assert!(refused(&seen));
        let seen = put_rule(&app, publ).unwrap();
        let (t, _) = give(&app, &seen, "").unwrap();
        assert!(app.auth.token(&t).is_some());
    }

    /// Keys kept fresh are used while their issuer is asked (for a key
    /// it does not have, say), not after that is answered.
    #[tokio::test]
    async fn kept_keys_do_not_wait_for_an_asking() {
        let url = "http://127.0.0.1:1";
        let iss = testutil::Issuer::new(url);
        let v = Verifier::default();
        let keys: Vec<Jwk> = serde_json::from_value(iss.jwks()["keys"].clone()).unwrap();
        v.keys
            .lock()
            .unwrap()
            .insert(url.into(), (Instant::now(), keys));
        let one = v
            .asking
            .lock()
            .unwrap()
            .entry(url.into())
            .or_default()
            .clone();
        let _asking = one.lock().await;
        let t = iss.token(json!({"aud": "pkgpod"}));
        let issuers = [url.to_string()];
        let r = tokio::time::timeout(Duration::from_secs(5), v.verify(&t, &issuers)).await;
        assert!(r.expect("it waited for the asking").is_ok());
    }

    /// An issuer that fails is not asked again for every token, whether
    /// they come at once or one after another.
    #[tokio::test]
    async fn failing_issuers_are_asked_seldom() {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let asked = Arc::new(AtomicUsize::new(0));
        let a2 = asked.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut r = BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                }
                a2.fetch_add(1, Ordering::SeqCst);
                // Slow, so tokens sent at once all come while it answers.
                std::thread::sleep(Duration::from_millis(300));
                let _ = write!(
                    conn,
                    "HTTP/1.1 500 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
            }
        });
        let v = Verifier::default();
        let t = testutil::Issuer::new(&url).token(json!({"aud": "pkgpod"}));
        let issuers = [url.clone()];
        let (a, b, c) = tokio::join!(
            v.verify(&t, &issuers),
            v.verify(&t, &issuers),
            v.verify(&t, &issuers)
        );
        for r in [a, b, c] {
            let e = r.unwrap_err().to_string();
            assert!(e.contains("500"), "{e}");
        }
        for _ in 0..3 {
            let e = v.verify(&t, &issuers).await.unwrap_err().to_string();
            assert!(e.contains("500"), "{e}");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn signatures() {
        let iss = testutil::Issuer::new("http://127.0.0.1:1");
        let t = iss.token(json!({"aud": "pkgpod"}));
        let keys: Vec<Jwk> = serde_json::from_value(iss.jwks()["keys"].clone()).unwrap();
        let (h, rest) = t.split_once('.').unwrap();
        let (p, s) = rest.split_once('.').unwrap();
        let input = format!("{h}.{p}");
        let sig = b64(s).unwrap();
        assert!(check("ES256", &keys[0], input.as_bytes(), &sig));
        // Another payload, or another algorithm named: no.
        assert!(!check("ES256", &keys[0], b"x.y", &sig));
        assert!(!check("RS256", &keys[0], input.as_bytes(), &sig));
        assert!(!check("none", &keys[0], input.as_bytes(), &[]));
        // RS256 with a key made here.
        let k = crate::auth::rsa_key::RsaKey::generate_for_tests();
        let jwk = Jwk {
            kty: "RSA".into(),
            kid: "r".into(),
            n: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(k.n()),
            e: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(k.e()),
            crv: String::new(),
            x: String::new(),
            y: String::new(),
        };
        let sig = k.sign(b"a.b").unwrap();
        assert!(check("RS256", &jwk, b"a.b", &sig));
        assert!(!check("RS256", &jwk, b"a.c", &sig));
    }
}
