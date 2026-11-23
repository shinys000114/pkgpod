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

/// The listener tasks running now.
#[derive(Default)]
pub struct Running {
    tasks: Mutex<HashMap<String, (Listener, watch::Sender<bool>)>>,
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
            "INSERT INTO listeners (name, addr, cert, key, enabled) VALUES (?1, ?2, ?3, ?4, ?5)
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
    tasks.retain(|name, (cfg, stop)| {
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
        tasks.insert(l.name.clone(), (l.clone(), tx));
        let app2 = app.clone();
        tokio::spawn(async move {
            let name = l.name.clone();
            let tls = if l.cert.is_empty() {
                None
            } else {
                match tls_acceptor(Path::new(&l.cert), Path::new(&l.key)) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        fail(&app2, &name, e);
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
            if let Err(e) = serve_router(&l.addr, tls, router, rx).await {
                fail(&app2, &name, e);
            }
        });
    }
}

fn fail(app: &App, name: &str, e: String) {
    tracing::error!("listener {name}: {e}");
    app.listeners
        .errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), e);
    app.listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(name);
}

pub fn stop_all(app: &App) {
    for (_, (_, stop)) in app
        .listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
    {
        let _ = stop.send(true);
    }
}

/// Serves `router` on `addr` until `stop` turns true.
pub async fn serve_router(
    addr: &str,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    // A listener changed in place: its old socket may take a moment to
    // close.
    let mut tries = 0;
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 20 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Err(e) => return Err(format!("listening on {addr}: {e}")),
        }
    };
    tracing::info!(
        "serving on {}{}",
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
    loop {
        let (tcp, _) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("accept on {addr}: {e}");
                    continue;
                }
            },
            _ = stop.wait_for(|s| *s) => return Ok(()),
        };
        let acceptor = acceptor.clone();
        let app = router.clone();
        let _ = tcp.set_nodelay(true);
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let service = hyper_util::service::TowerToHyperService::new(app);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
        });
    }
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
