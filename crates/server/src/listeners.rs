// SPDX-License-Identifier: AGPL-3.0-only

//! Serving listeners: addresses (plain HTTP, or TLS with certificate
//! files) on which repositories are served. They are kept in the
//! database and started, changed and stopped while the server runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use pkgpod_base::db::Db;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{App, Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listener {
    pub name: String,
    /// `host:port` to bind.
    pub addr: String,
    /// PEM certificate chain and key; both empty: plain HTTP.
    #[serde(default)]
    pub cert: String,
    #[serde(default)]
    pub key: String,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// A running listener: its configuration, stop signal and generation.
type Task = (Listener, watch::Sender<bool>, u64);

/// The listener tasks running now.
#[derive(Default)]
pub struct Running {
    /// By name: the configuration, the stop signal, and the task's
    /// generation (a task that ends tells only on its own behalf).
    tasks: Mutex<HashMap<String, Task>>,
    /// The tasks of listeners told to stop, until they have drained.
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    errors: Mutex<HashMap<String, String>>,
}

impl Running {
    /// Why a listener is not running, if it failed.
    pub fn error(&self, name: &str) -> Option<String> {
        self.errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    pub fn is_running(&self, name: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }
}

/// On a new database: one listener on 127.0.0.1:8080, for a reverse
/// proxy in front.
pub fn ensure_default(db: &Db) -> Result<()> {
    if crate::keys::get_setting(db, "listeners_initialized")?.is_some() {
        return Ok(());
    }
    db.write(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO listeners (name, addr) VALUES ('default', '127.0.0.1:8080')",
            [],
        )
    })?;
    crate::keys::set_setting(db, "listeners_initialized", Some("1"))
}

pub fn list(db: &Db) -> Result<Vec<Listener>> {
    Ok(db.read(|c| {
        let mut st =
            c.prepare("SELECT name, addr, cert, key, enabled FROM listeners ORDER BY name")?;
        let v = st
            .query_map([], |r| {
                Ok(Listener {
                    name: r.get(0)?,
                    addr: r.get(1)?,
                    cert: r.get(2)?,
                    key: r.get(3)?,
                    enabled: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok::<_, rusqlite::Error>(v)
    })?)
}

pub fn set(db: &Db, l: &Listener) -> Result<()> {
    pkgpod_engine::check_name("listener", &l.name).map_err(|e| Error::Invalid(e.to_string()))?;
    if l.addr.parse::<std::net::SocketAddr>().is_err() && !l.addr.contains(':') {
        return Err(Error::Invalid(format!(
            "{:?}: an address is host:port",
            l.addr
        )));
    }
    if l.cert.is_empty() != l.key.is_empty() {
        return Err(Error::Invalid(
            "TLS needs both a certificate and a key".into(),
        ));
    }
    if !l.cert.is_empty() {
        tls_acceptor(Path::new(&l.cert), Path::new(&l.key)).map_err(Error::Invalid)?;
    }
    db.write(|tx| {
        tx.execute(
            "INSERT INTO listeners (name, addr, cert, key, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET addr = excluded.addr, cert = excluded.cert,
               key = excluded.key, enabled = excluded.enabled",
            params![l.name, l.addr, l.cert, l.key, l.enabled],
        )
    })?;
    Ok(())
}

pub fn remove(db: &Db, name: &str) -> Result<()> {
    let n = db.write(|tx| tx.execute("DELETE FROM listeners WHERE name = ?1", [name]))?;
    if n == 0 {
        return Err(Error::NotFound(format!("no listener {name}")));
    }
    Ok(())
}

/// Starts, restarts and stops listener tasks to match the database.
pub fn reconcile(app: &Arc<App>) {
    let want = match list(&app.db) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("listeners: {e}");
            return;
        }
    };
    let mut tasks = app
        .listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    tasks.retain(|name, (cfg, stop, _)| {
        let keep = want.iter().any(|w| w == cfg && w.enabled);
        if !keep {
            tracing::info!("listener {name}: stopping");
            let _ = stop.send(true);
        }
        keep
    });
    for l in want.into_iter().filter(|l| l.enabled) {
        if tasks.contains_key(&l.name) {
            continue;
        }
        let (tx, rx) = watch::channel(false);
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tasks.insert(l.name.clone(), (l.clone(), tx, generation));
        let app2 = app.clone();
        let h = tokio::spawn(async move {
            let name = l.name.clone();
            let tls = if l.cert.is_empty() {
                None
            } else {
                match tls_acceptor(Path::new(&l.cert), Path::new(&l.key)) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        fail(&app2, &name, generation, e);
                        return;
                    }
                }
            };
            let router = crate::serve::router(app2.clone());
            app2.listeners
                .errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&name);
            if let Err(e) = serve_router(
                &format!("listener {}: serving", l.name),
                &l.addr,
                tls,
                router,
                rx,
            )
            .await
            {
                fail(&app2, &name, generation, e);
            }
        });
        let mut hs = app
            .listeners
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        hs.retain(|h| !h.is_finished());
        hs.push(h);
    }
}

/// A listener task of `generation` could not serve: recorded, and its
/// entry dropped, unless the listener has been started anew since (a
/// task still retrying an old address must not stop its successor).
fn fail(app: &App, name: &str, generation: u64, e: String) {
    let mut tasks = app
        .listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if tasks.get(name).is_some_and(|t| t.2 != generation) {
        tracing::debug!("listener {name}: an earlier start ended: {e}");
        return;
    }
    tracing::error!("listener {name}: {e}");
    tasks.remove(name);
    app.listeners
        .errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), e);
}

/// Tells every listener to stop; returns their tasks, which end once
/// the requests in flight are answered.
pub fn stop_all(app: &App) -> Vec<tokio::task::JoinHandle<()>> {
    for (_, (_, stop, _)) in app
        .listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
    {
        let _ = stop.send(true);
    }
    std::mem::take(
        &mut *app
            .listeners
            .handles
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    )
}

/// Binds `addr`, waiting a little if it is in use: a listener changed
/// in place may take a moment to close its old socket.
/// Gives up at once when `stop` turns true meanwhile.
pub async fn bind(
    addr: &str,
    stop: &watch::Receiver<bool>,
) -> Result<tokio::net::TcpListener, String> {
    let mut tries = 0;
    loop {
        if *stop.borrow() {
            return Err(format!("{addr}: stopped before it could listen"));
        }
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 20 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(format!(
                    "listening on {addr}: {e}; another program has that address"
                ));
            }
            Err(e) => return Err(format!("listening on {addr}: {e}")),
        }
    }
}

/// Serves `router` on `addr` until `stop` turns true; then accepts no
/// more connections, closes idle ones and returns once the requests in
/// flight are answered.
pub async fn serve_router(
    what: &str,
    addr: &str,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let listener = bind(addr, &stop).await.map_err(|e| {
        if !e.ends_with("another program has that address") {
            e
        } else if what.starts_with("management") {
            format!("{e} (choose another with listen in the configuration file)")
        } else {
            format!("{e} (choose another with pkgpod listener set)")
        }
    })?;
    tracing::info!(
        "{what} on {}{}",
        listener
            .local_addr()
            .map_or(addr.to_string(), |a| a.to_string()),
        if tls.is_some() { " (TLS)" } else { "" }
    );
    let Some(acceptor) = tls else {
        // Without TCP_NODELAY a small file (headers and body written
        // apart) waits for the client's delayed ACK: 40 ms a request.
        use axum::serve::ListenerExt;
        let listener = listener.tap_io(|tcp| {
            let _ = tcp.set_nodelay(true);
        });
        return axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stop.wait_for(|s| *s).await;
        })
        .await
        .map_err(|e| format!("serving: {e}"));
    };
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    loop {
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("accept on {addr}: {e}");
                    continue;
                }
            },
            _ = stop.wait_for(|s| *s) => break,
        };
        let acceptor = acceptor.clone();
        let app = router.clone();
        let watcher = graceful.watcher();
        let _ = tcp.set_nodelay(true);
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            // The peer address, as the plain listener gives it.
            let app = app.layer(axum::Extension(axum::extract::ConnectInfo(peer)));
            let service = hyper_util::service::TowerToHyperService::new(app);
            let builder =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            let conn = builder.serve_connection(hyper_util::rt::TokioIo::new(tls), service);
            let _ = watcher.watch(conn).await;
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// A TLS acceptor from PEM files.
pub fn tls_acceptor(cert: &Path, key: &Path) -> Result<tokio_rustls::TlsAcceptor, String> {
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS certificate: {e}"))?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Paths a listener names, for checks.
pub fn files(l: &Listener) -> Vec<PathBuf> {
    [&l.cert, &l.key]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}
