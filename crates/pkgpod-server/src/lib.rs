// SPDX-License-Identifier: AGPL-3.0-only

//! The pkgpod server: serving listeners (with host routing and protocol
//! writes), the management listener (JSON API and web console), rights,
//! signing keys and background jobs.

pub mod api;
pub mod auth;
pub mod config;
pub mod keys;
pub mod listeners;
pub mod routes;
pub mod schema;
pub mod serve;

mod console;
mod web;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pkgpod_base::db::Db;
use pkgpod_engine::{Engine, Plan, RepoInfo, Version};

use crate::auth::Auth;
use crate::config::Config;
use crate::keys::Keys;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("authentication required")]
    Unauthorized,
    #[error(transparent)]
    Engine(#[from] pkgpod_engine::Error),
    #[error(transparent)]
    Base(#[from] pkgpod_base::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn status(&self) -> StatusCode {
        use pkgpod_engine::Error as E;
        match self {
            Error::Invalid(_) | Error::Engine(E::Invalid(_)) => StatusCode::BAD_REQUEST,
            Error::NotFound(_) | Error::Engine(E::NotFound(_)) => StatusCode::NOT_FOUND,
            Error::Conflict(_) | Error::Engine(E::Conflict(_)) => StatusCode::CONFLICT,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let st = self.status();
        if st == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!("{self}");
        }
        let mut r = (
            st,
            axum::Json(serde_json::json!({"error": self.to_string()})),
        )
            .into_response();
        if st == StatusCode::UNAUTHORIZED {
            r.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Basic realm=\"pkgpod\""),
            );
        }
        r
    }
}

/// A plan kept for applying later.
pub struct Kept {
    pub created: i64,
    pub owner: String,
    pub plan: Plan,
}

/// What a serving request needs to know about a repository, cached per
/// engine generation.
#[derive(Clone)]
pub struct Served {
    pub repo: RepoInfo,
    pub current: Option<Version>,
}

pub struct App {
    pub cfg: Config,
    pub db: Arc<Db>,
    pub engine: Arc<Engine>,
    pub auth: Auth,
    pub keys: Arc<Keys>,
    pub listeners: listeners::Running,
    pub plans: Mutex<HashMap<String, Kept>>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    generation: u64,
    routes: Option<Arc<routes::Table>>,
    repos: HashMap<String, Served>,
    labels: HashMap<(String, String), Version>,
}

/// Formats compiled into this server.
pub fn formats() -> Vec<Arc<dyn pkgpod_engine::Format>> {
    vec![Arc::new(pkgpod_deb::Deb)]
}

impl App {
    /// Opens the database (creating it), the engine and the tables; makes
    /// the first admin if there are no users.
    pub fn open(cfg: Config) -> Result<Arc<App>> {
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|e| Error::Invalid(format!("{}: {e}", cfg.data_dir.display())))?;
        let db = Arc::new(Db::open(&cfg.db_path())?);
        schema::migrate(&db)?;
        let keys = Arc::new(Keys::new(db.clone(), &cfg.gnupg()));
        let engine = Arc::new(Engine::open(
            db.clone(),
            &cfg.default_storage(),
            formats(),
            keys.clone(),
        )?);
        let auth = Auth::new(db.clone());
        let pw = cfg.data_dir.join("admin-password");
        if auth.bootstrap(&pw)? {
            tracing::warn!(
                "created the user admin; its password is in {} (change it, then delete the file)",
                pw.display()
            );
        }
        listeners::ensure_default(&db)?;
        Ok(Arc::new(App {
            cfg,
            db,
            engine,
            auth,
            keys,
            listeners: listeners::Running::default(),
            plans: Mutex::new(HashMap::new()),
            cache: Mutex::new(Cache::default()),
        }))
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let g = self.engine.generation();
        if c.generation != g {
            *c = Cache {
                generation: g,
                ..Default::default()
            };
        }
        c
    }

    pub fn routes(&self) -> Result<Arc<routes::Table>> {
        if let Some(t) = &self.cache().routes {
            return Ok(t.clone());
        }
        let t = Arc::new(routes::Table::build(&self.engine.repos()?));
        self.cache().routes = Some(t.clone());
        Ok(t)
    }

    pub fn served(&self, repo: &str) -> Result<Served> {
        if let Some(s) = self.cache().repos.get(repo) {
            return Ok(s.clone());
        }
        let r = self.engine.repo(repo)?;
        let s = Served {
            current: self.engine.current(&r)?,
            repo: r,
        };
        self.cache().repos.insert(repo.to_string(), s.clone());
        Ok(s)
    }

    pub fn labelled(&self, repo: &str, label: &str) -> Result<Version> {
        let k = (repo.to_string(), label.to_string());
        if let Some(v) = self.cache().labels.get(&k) {
            return Ok(v.clone());
        }
        let v = self.engine.resolve(repo, label)?;
        self.cache().labels.insert(k, v.clone());
        Ok(v)
    }

    /// Keeps a plan for an hour; returns its id.
    pub fn keep_plan(&self, owner: &str, plan: Plan) -> String {
        let id = pkgpod_base::random_hex(12);
        let now = pkgpod_base::now();
        let mut p = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        p.retain(|_, k| k.created > now - 3600);
        p.insert(
            id.clone(),
            Kept {
                created: now,
                owner: owner.to_string(),
                plan,
            },
        );
        id
    }

    pub fn take_plan(&self, id: &str, owner: &str) -> Result<Plan> {
        let mut p = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        match p.get(id) {
            Some(k) if k.owner == owner => Ok(p.remove(id).expect("present").plan),
            Some(_) => Err(Error::Forbidden("that plan is someone else's".into())),
            None => Err(Error::NotFound(format!(
                "no plan {id} (plans are kept for an hour)"
            ))),
        }
    }
}

/// Runs the server until SIGINT or SIGTERM: the management listener, the
/// serving listeners in the database, and the background jobs.
pub async fn run(app: Arc<App>) -> Result<(), String> {
    listeners::reconcile(&app);
    let gc = {
        let app = app.clone();
        tokio::spawn(async move {
            // Daily, the first time an hour after start.
            let mut every = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(3600),
                Duration::from_secs(86400),
            );
            loop {
                every.tick().await;
                let e = app.engine.clone();
                match tokio::task::spawn_blocking(move || e.gc(Duration::from_secs(86400), false))
                    .await
                {
                    Ok(Ok(r)) => tracing::info!(
                        "gc: {} versions, {} objects, {} blobs ({} bytes) removed",
                        r.versions,
                        r.objects,
                        r.blobs,
                        r.bytes
                    ),
                    Ok(Err(e)) => tracing::error!("gc: {e}"),
                    Err(e) => tracing::error!("gc: {e}"),
                }
            }
        })
    };
    let mgmt = web::router(app.clone());
    let addr = app.cfg.listen.clone();
    let tls = match (&app.cfg.tls_cert, &app.cfg.tls_key) {
        (Some(c), Some(k)) => Some(listeners::tls_acceptor(c, k)?),
        _ => None,
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    });
    tracing::info!("pkgpod {}: management on {addr}", pkgpod_base::version());
    let r = listeners::serve_router(&addr, tls, mgmt, stop_rx).await;
    listeners::stop_all(&app);
    gc.abort();
    r
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}
