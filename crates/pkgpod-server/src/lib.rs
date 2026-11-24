// SPDX-License-Identifier: AGPL-3.0-only

//! The pkgpod server: serving listeners (with host routing and protocol
//! writes), the management listener (JSON API and web console), rights,
//! signing keys and background jobs.

pub mod api;
pub mod auth;
pub mod backup;
pub mod cargo;
pub mod composer;
pub mod conan;
pub mod config;
pub mod galaxy;
pub mod gems;
pub mod go;
pub mod helm;
pub mod import;
pub mod jobs;
pub mod keys;
pub mod listeners;
pub mod npm;
pub mod nuget;
pub mod oci;
pub mod openapi;
pub mod promote;
pub mod pub_api;
pub mod pypi;
pub mod resign;
pub mod routes;
pub mod rsa_key;
pub mod schema;
pub mod serve;
pub mod swift;
pub mod terraform;
pub mod usign_key;

mod console;
mod cron;
mod mirror_pkgs;
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
    pub jobs: jobs::Jobs,
    pub plans: Mutex<HashMap<String, Kept>>,
    /// Conan revisions being uploaded: their files before the manifest.
    pub conan_staged: conan::Staged,
    pub resigner: resign::Resigner,
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
    vec![
        Arc::new(pkgpod_deb::Deb),
        Arc::new(pkgpod_files::Files),
        Arc::new(pkgpod_oci::Oci),
        Arc::new(pkgpod_pypi::Pypi),
        Arc::new(pkgpod_npm::Npm),
        Arc::new(pkgpod_cargo::Cargo),
        Arc::new(pkgpod_go::Go),
        Arc::new(pkgpod_apk::Apk),
        Arc::new(pkgpod_rpm::Rpm),
        Arc::new(pkgpod_helm::Helm),
        Arc::new(pkgpod_maven::Maven),
        Arc::new(pkgpod_gems::Gems),
        Arc::new(pkgpod_arch::Arch),
        Arc::new(pkgpod_nuget::Nuget),
        Arc::new(pkgpod_composer::Composer),
        Arc::new(pkgpod_conda::Conda),
        Arc::new(pkgpod_cran::Cran),
        Arc::new(pkgpod_pub::Pub),
        Arc::new(pkgpod_ipk::Ipk),
        Arc::new(pkgpod_terraform::Terraform),
        Arc::new(pkgpod_galaxy::Galaxy),
        Arc::new(pkgpod_conan::Conan),
        Arc::new(pkgpod_swift::Swift),
    ]
}

impl App {
    /// Opens the database (creating it), the engine and the tables; makes
    /// the first admin if there are no users.
    pub fn open(cfg: Config) -> Result<Arc<App>> {
        if !cfg.data_dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(&cfg.data_dir)
                .map_err(|e| Error::Invalid(format!("{}: {e}", cfg.data_dir.display())))?;
        }
        let db = Arc::new(Db::open(&cfg.db_path())?);
        schema::migrate(&db)?;
        let keys = Arc::new(Keys::new(
            db.clone(),
            &cfg.data_dir.join("keys"),
            &cfg.data_dir.join("rsa"),
        ));
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
            jobs: jobs::Jobs::default(),
            plans: Mutex::new(HashMap::new()),
            conan_staged: conan::Staged::default(),
            resigner: resign::Resigner::default(),
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
    // Only the running server owns jobs: admin commands open the same
    // database while a server runs.
    jobs::recover(&app.db).map_err(|e| e.to_string())?;
    listeners::reconcile(&app);
    let sched = jobs::scheduler(app.clone());
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
                    Ok(Ok(r)) => {
                        tracing::info!(
                            "gc: {} versions, {} objects, {} blobs ({} bytes) removed",
                            r.versions,
                            r.objects,
                            r.blobs,
                            r.bytes
                        );
                        app.auth.audit(
                            "pkgpod",
                            "gc",
                            "",
                            &format!("daily: {} blobs, {} bytes", r.blobs, r.bytes),
                        );
                    }
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
    tracing::info!("pkgpod {}: management on {addr}", pkgpod_base::version());
    let mut mgmt =
        tokio::spawn(async move { listeners::serve_router(&addr, tls, mgmt, stop_rx).await });
    tokio::select! {
        r = &mut mgmt => {
            // The management listener could not start (or failed).
            listeners::stop_all(&app);
            app.jobs.cancel_all();
            return r.unwrap_or_else(|e| Err(e.to_string()));
        }
        _ = shutdown_signal() => {}
    }
    // Every listener at once: no new connections, idle ones closed, the
    // requests in flight (a large download, say) answered, up to DRAIN.
    tracing::info!(
        "stopping: answering the requests in flight (at most {}s)",
        DRAIN.as_secs()
    );
    let _ = stop_tx.send(true);
    let serving = listeners::stop_all(&app);
    app.jobs.cancel_all();
    sched.abort();
    gc.abort();
    let drained = tokio::time::timeout(DRAIN, async move {
        let r = mgmt.await;
        for h in serving {
            let _ = h.await;
        }
        r
    })
    .await;
    match drained {
        Ok(r) => r.unwrap_or_else(|e| Err(e.to_string())),
        Err(_) => {
            tracing::warn!(
                "stopping: requests still in flight after {}s are cut",
                DRAIN.as_secs()
            );
            Ok(())
        }
    }
}

/// How long a stopping server answers the requests in flight.
pub const DRAIN: Duration = Duration::from_secs(30);

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
}
