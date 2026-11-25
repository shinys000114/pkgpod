// SPDX-License-Identifier: AGPL-3.0-only

//! Serving listeners: addresses (plain HTTP, or TLS with certificate
//! files) on which repositories are served. They are kept in the
//! database and started, changed and stopped while the server runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// A running listener: its configuration, stop signal, generation and
/// whether it listens yet (it may wait for its address).
type Task = (Listener, watch::Sender<bool>, u64, bool);

/// The listener tasks running now.
#[derive(Default)]
pub struct Running {
    /// By name: the configuration, the stop signal, the task's
    /// generation (a task that ends tells only on its own behalf), and
    /// whether it listens.
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

    /// Whether a listener listens on its address.
    pub fn is_running(&self, name: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .is_some_and(|t| t.3)
    }

    /// Whether a listener is started but does not listen yet: its
    /// address may be held a while (one moving off it), or for good.
    pub fn is_starting(&self, name: &str) -> bool {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .is_some_and(|t| !t.3)
    }
}

/// On a new database: one listener on 127.0.0.1:8080, for a reverse
/// proxy in front.
pub fn ensure_default(db: &Db) -> Result<()> {
    if crate::auth::keys::get_setting(db, "listeners_initialized")?.is_some() {
        return Ok(());
    }
    db.write(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO listeners (name, addr) VALUES ('default', '127.0.0.1:8080')",
            [],
        )
    })?;
    crate::auth::keys::set_setting(db, "listeners_initialized", Some("1"))
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
    tasks.retain(|name, (cfg, stop, ..)| {
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
        tasks.insert(l.name.clone(), (l.clone(), tx, generation, false));
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
            let router = crate::serving::router(app2.clone());
            app2.listeners
                .errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&name);
            if let Err(e) = serve(
                &format!("listener {}: serving", l.name),
                &l.addr,
                tls,
                router,
                rx,
                || listening(&app2, &name, generation),
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

/// A listener task of `generation` listens: unless the listener has
/// been started anew since (as in [`fail`]).
fn listening(app: &App, name: &str, generation: u64) {
    let mut tasks = app
        .listeners
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(t) = tasks.get_mut(name).filter(|t| t.2 == generation) {
        t.3 = true;
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
    for (_, (_, stop, ..)) in app
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

/// How long a client has for the TLS handshake, for its first bytes
/// (and an HTTP/2 client to answer a ping), and for each request's
/// headers: connections that send nothing would otherwise hold their
/// file descriptors for ever.
///
/// hyper times an idle kept-alive HTTP/1 connection's wait for its next
/// request as part of that request's headers. A reverse proxy keeps
/// idle connections to its upstream (Caddy two minutes, nginx one) and
/// takes one for the next request; one this server closes at that
/// moment fails, and a request with a body is not sent again but
/// answered 502. So the headers have longer than proxies keep idle.
const HANDSHAKE: Duration = Duration::from_secs(10);
const FIRST: Duration = Duration::from_secs(30);
const HEADERS: Duration = Duration::from_secs(180);
const _: () = assert!(HEADERS.as_secs() > 120, "longer than Caddy keeps idle");

/// Serves `router` on `addr` until `stop` turns true; then accepts no
/// more connections, closes idle ones and returns once the requests in
/// flight are answered.
pub async fn serve_router(
    what: &str,
    addr: &str,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    stop: watch::Receiver<bool>,
) -> Result<(), String> {
    serve(what, addr, tls, router, stop, || {}).await
}

/// [`serve_router`], calling `listening` once `addr` is bound.
async fn serve(
    what: &str,
    addr: &str,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    stop: watch::Receiver<bool>,
    listening: impl FnOnce(),
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
    listening();
    tracing::info!(
        "{what} on {}{}",
        listener
            .local_addr()
            .map_or(addr.to_string(), |a| a.to_string()),
        if tls.is_some() { " (TLS)" } else { "" }
    );
    let limits = Limits {
        handshake: HANDSHAKE,
        first: FIRST,
        headers: HEADERS,
    };
    serve_on(listener, tls, router, stop, limits).await;
    Ok(())
}

/// See [`HANDSHAKE`], [`FIRST`] and [`HEADERS`].
#[derive(Clone, Copy)]
struct Limits {
    handshake: Duration,
    first: Duration,
    headers: Duration,
}

async fn serve_on(
    listener: tokio::net::TcpListener,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: Router,
    mut stop: watch::Receiver<bool>,
    limits: Limits,
) {
    let Limits {
        handshake,
        first,
        headers,
    } = limits;
    let addr = listener
        .local_addr()
        .map_or_else(|_| "?".to_string(), |a| a.to_string());
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(headers);
    // HTTP/2 has no header timeout: past its preface, a connection that
    // sends nothing more would be held for ever. One that does not
    // answer a ping in time is closed (a client in the middle of a long
    // download answers them).
    builder
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .keep_alive_interval(first)
        .keep_alive_timeout(first);
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    let mut warned: Option<tokio::time::Instant> = None;
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = stop.wait_for(|s| *s) => break,
        };
        let (tcp, peer) = match accepted {
            Ok(c) => c,
            // The client gave up before it was accepted.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                continue;
            }
            Err(e) => {
                // Out of file descriptors, say: the next accept fails at
                // once too. Wait for some to be closed, and say so once a
                // minute, not a million times a second.
                if warned.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                    tracing::warn!("accept on {addr}: {e}");
                    warned = Some(tokio::time::Instant::now());
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
                    _ = stop.wait_for(|s| *s) => break,
                }
            }
        };
        // Without TCP_NODELAY a small file (headers and body written
        // apart) waits for the client's delayed ACK: 40 ms a request.
        let _ = tcp.set_nodelay(true);
        let acceptor = tls.clone();
        // The peer address, as axum's ConnectInfo gives it.
        let app = tower::Layer::layer(
            &axum::Extension(axum::extract::ConnectInfo(peer)),
            router.clone(),
        );
        let watcher = graceful.watcher();
        let builder = builder.clone();
        tokio::spawn(async move {
            let service = hyper_util::service::TowerToHyperService::new(app);
            match acceptor {
                Some(a) => {
                    let Ok(Ok(tls)) = tokio::time::timeout(handshake, a.accept(tcp)).await else {
                        return;
                    };
                    let io = hyper_util::rt::TokioIo::new(FirstBytes::new(tls, first));
                    let _ = watcher.watch(builder.serve_connection(io, service)).await;
                }
                None => {
                    let io = hyper_util::rt::TokioIo::new(FirstBytes::new(tcp, first));
                    let _ = watcher.watch(builder.serve_connection(io, service)).await;
                }
            }
        });
    }
    drop(listener);
    graceful.shutdown().await;
}

/// A connection, until its first bytes show which HTTP it speaks: hyper
/// reads them to tell HTTP/2's preface from HTTP/1 with no timeout of
/// its own (its header timeout starts after), so a client that sends
/// nothing would hold the connection for ever.
struct FirstBytes<I> {
    io: I,
    /// Until the bytes read stop being the start of the preface.
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    seen: usize,
}

impl<I> FirstBytes<I> {
    fn new(io: I, within: Duration) -> Self {
        FirstBytes {
            io,
            deadline: Some(Box::pin(tokio::time::sleep(within))),
            seen: 0,
        }
    }
}

impl<I: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for FirstBytes<I> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::future::Future;
        use std::task::Poll;
        const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut this.io).poll_read(cx, buf);
        let Some(deadline) = this.deadline.as_mut() else {
            return r;
        };
        match r {
            Poll::Ready(Ok(())) => {
                let new = &buf.filled()[before..];
                let preface = PREFACE.get(this.seen..).unwrap_or(&[]);
                this.seen += new.len();
                if new.is_empty()
                    || this.seen >= PREFACE.len()
                    || new.iter().zip(preface).any(|(a, b)| a != b)
                {
                    this.deadline = None;
                }
            }
            Poll::Pending if deadline.as_mut().poll(cx).is_ready() => {
                return Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
            }
            _ => {}
        }
        r
    }
}

impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for FirstBytes<I> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
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

/// Whether listening on `a` would take the port of one listening on
/// `b`: the same address, or an unspecified one that covers the other.
/// [`bind`] leaves IPV6_V6ONLY as the system has it, off on Linux, so
/// `[::]` takes IPv4 too, while `0.0.0.0` takes only IPv4 (and does not
/// clash with `[::1]`). A mapped address is its IPv4 one.
pub fn clash(a: std::net::SocketAddr, b: std::net::SocketAddr) -> bool {
    let covers = |x: std::net::IpAddr, y: std::net::IpAddr| {
        x == y || (x.is_unspecified() && (x.is_ipv6() || y.is_ipv4()))
    };
    let (x, y) = (a.ip().to_canonical(), b.ip().to_canonical());
    a.port() == b.port() && (covers(x, y) || covers(y, x))
}

/// Paths a listener names, for checks.
pub fn files(l: &Listener) -> Vec<PathBuf> {
    [&l.cert, &l.key]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::time::Instant;

    use super::*;

    /// A listener answering "ok", with short timeouts.
    async fn serve(
        tls: Option<tokio_rustls::TlsAcceptor>,
    ) -> (std::net::SocketAddr, watch::Sender<bool>) {
        let t = Duration::from_millis(300);
        serve_with(tls, t, t).await
    }

    /// The same, with `first` for the first bytes and `headers` for
    /// each request's headers.
    async fn serve_with(
        tls: Option<tokio_rustls::TlsAcceptor>,
        first: Duration,
        headers: Duration,
    ) -> (std::net::SocketAddr, watch::Sender<bool>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        let slowly = || async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            "slow"
        };
        // A body in two parts, a second apart.
        let streamed = || async {
            let parts = futures_util::stream::unfold(0, |i| async move {
                if i == 2 {
                    return None;
                }
                if i == 1 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from("part\n")),
                    i + 1,
                ))
            });
            axum::body::Body::from_stream(parts)
        };
        let router = Router::new()
            .route("/", axum::routing::get(|| async { "ok" }))
            .route("/slow", axum::routing::get(slowly))
            .route("/stream", axum::routing::get(streamed))
            .route("/upload", axum::routing::post(|b: String| async move { b }));
        let limits = Limits {
            handshake: first,
            first,
            headers,
        };
        tokio::spawn(serve_on(l, tls, router, rx, limits));
        (a, tx)
    }

    /// Whether the server closes `c` within a few seconds.
    fn closed(c: &mut std::net::TcpStream) -> bool {
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let t = Instant::now();
        let mut buf = [0u8; 64];
        matches!(c.read(&mut buf), Ok(0)) && t.elapsed() < Duration::from_secs(4)
    }

    /// Whether the server closes `c` within a few seconds, whatever it
    /// says before.
    fn closed_after(c: &mut std::net::TcpStream) -> bool {
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let t = Instant::now();
        let mut buf = [0u8; 256];
        while let Ok(n) = c.read(&mut buf) {
            if n == 0 {
                return t.elapsed() < Duration::from_secs(4);
            }
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connections_that_send_nothing_are_closed() {
        let (a, _stop) = serve(None).await;
        tokio::task::spawn_blocking(move || {
            let mut c = std::net::TcpStream::connect(a).unwrap();
            assert!(closed(&mut c), "never a request");
            // Answered, then idle.
            let mut c = std::net::TcpStream::connect(a).unwrap();
            c.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
            let mut buf = [0u8; 512];
            let n = c.read(&mut buf).unwrap();
            assert!(buf[..n].starts_with(b"HTTP/1.1 200"), "{:?}", &buf[..n]);
            assert!(closed(&mut c), "kept alive, idle");
            // Headers that never end.
            let mut c = std::net::TcpStream::connect(a).unwrap();
            c.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n").unwrap();
            assert!(closed(&mut c), "half a request");
            // HTTP/2's preface, then nothing.
            let mut c = std::net::TcpStream::connect(a).unwrap();
            c.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").unwrap();
            assert!(closed_after(&mut c), "HTTP/2, idle");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kept_alive_longer_than_the_first_bytes_have() {
        // As a reverse proxy does: a request, a while idle, then the next
        // with a body on the same connection.
        let (a, _stop) = serve_with(None, Duration::from_millis(300), Duration::from_secs(5)).await;
        tokio::task::spawn_blocking(move || {
            let mut c = std::net::TcpStream::connect(a).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0u8; 512];
            c.write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
            let n = c.read(&mut buf).unwrap();
            assert!(buf[..n].starts_with(b"HTTP/1.1 200"), "{:?}", &buf[..n]);
            std::thread::sleep(Duration::from_secs(1));
            c.write_all(b"POST /upload HTTP/1.1\r\nhost: x\r\ncontent-length: 4\r\n\r\nbody")
                .unwrap();
            let n = c.read(&mut buf).unwrap();
            let r = String::from_utf8_lossy(&buf[..n]);
            assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("body"), "{r}");
        })
        .await
        .unwrap();
    }

    /// What the server answers to `req`, sent in parts a second apart.
    fn exchange(a: std::net::SocketAddr, req: &[&str]) -> String {
        let mut c = std::net::TcpStream::connect(a).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        for (i, part) in req.iter().enumerate() {
            if i > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            c.write_all(part.as_bytes()).unwrap();
        }
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_longer_than_the_timeouts_are_answered() {
        let (a, _stop) = serve(None).await;
        tokio::task::spawn_blocking(move || {
            let close = "host: x\r\nconnection: close\r\n";
            // A slow answer, a body streamed slowly, a body sent slowly.
            let r = exchange(a, &[&format!("GET /slow HTTP/1.1\r\n{close}\r\n")]);
            assert!(r.starts_with("HTTP/1.1 200") && r.ends_with("slow"), "{r}");
            let r = exchange(a, &[&format!("GET /stream HTTP/1.1\r\n{close}\r\n")]);
            assert!(
                r.starts_with("HTTP/1.1 200")
                    && r.ends_with("5\r\npart\n\r\n5\r\npart\n\r\n0\r\n\r\n"),
                "{r}"
            );
            let r = exchange(
                a,
                &[
                    &format!("POST /upload HTTP/1.1\r\n{close}content-length: 10\r\n\r\nfirst"),
                    "later",
                ],
            );
            assert!(
                r.starts_with("HTTP/1.1 200") && r.ends_with("firstlater"),
                "{r}"
            );
        })
        .await
        .unwrap();
        // Over HTTP/2 too: the client answers the pings.
        let c = reqwest::Client::builder()
            .http2_prior_knowledge()
            .build()
            .unwrap();
        for (path, want) in [("slow", "slow"), ("stream", "part\npart\n")] {
            let r = c.get(format!("http://{a}/{path}")).send().await.unwrap();
            assert_eq!(r.version(), reqwest::Version::HTTP_2);
            assert_eq!(r.text().await.unwrap(), want);
        }
    }

    #[test]
    fn addresses_clash_as_binding_them_does() {
        // As Linux binds them (IPV6_V6ONLY off): [::] takes IPv4 too,
        // 0.0.0.0 only IPv4; a mapped address is its IPv4 one.
        for (a, b, clashes) in [
            ("0.0.0.0:80", "[::1]:80", false),
            ("127.0.0.1:80", "[::1]:80", false),
            ("0.0.0.0:80", "0.0.0.0:81", false),
            ("10.0.0.1:80", "10.0.0.2:80", false),
            ("127.0.0.1:80", "127.0.0.1:80", true),
            ("0.0.0.0:80", "127.0.0.1:80", true),
            ("[::]:80", "127.0.0.1:80", true),
            ("[::]:80", "0.0.0.0:80", true),
            ("[::]:80", "[::1]:80", true),
            ("[::ffff:127.0.0.1]:80", "127.0.0.1:80", true),
        ] {
            let (a, b) = (a.parse().unwrap(), b.parse().unwrap());
            assert_eq!(clash(a, b), clashes, "{a} {b}");
            assert_eq!(clash(b, a), clashes, "{b} {a}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tls_handshake_that_never_comes_is_cut() {
        let d = tempfile::tempdir().unwrap();
        let (cert, key) = (d.path().join("cert.pem"), d.path().join("key.pem"));
        let made = std::process::Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            ])
            .args(["-subj", "/CN=127.0.0.1", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            eprintln!("skipping: openssl not installed");
            return;
        }
        let (a, _stop) = serve(Some(tls_acceptor(&cert, &key).unwrap())).await;
        tokio::task::spawn_blocking(move || {
            let mut c = std::net::TcpStream::connect(a).unwrap();
            assert!(closed(&mut c));
        })
        .await
        .unwrap();
    }
}
