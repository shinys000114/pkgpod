// SPDX-License-Identifier: AGPL-3.0-only

//! The OCI registry's side in the server: its protocol is pkgpod-oci's
//! ([`pkgpod_oci::protocol`]); here are what the server lends it (the
//! App as its [`Host`], the routes' names, the upstream registries'
//! clients) and which requests are its.

mod host;
mod upstream;

pub use pkgpod_oci::protocol::{
    Fetch, Fetched, Host, Names, State, Upstream, forget, handle, pin_closure,
};
pub(crate) use upstream::Registries;

/// Is `/v2/<rest>` on `host` the registry's: a name that resolves to an
/// OCI repository, or the bare `/v2/` (the version check clients make
/// first)? That is the registry's where an OCI repository has an address
/// on the host, or where one without addresses is reachable and no other
/// repository's address covers `/v2`. Otherwise the request is routed as
/// any other (a file tree may have a `v2/`).
pub fn claims(table: &crate::routes::Table, host: &str, rest: &[String]) -> bool {
    if rest.is_empty() {
        if table.has_addressed_registry(host) {
            return true;
        }
        return table.has_registry(host)
            && !matches!(
                table.route(host, &["v2".to_string()]),
                crate::routes::Hit::Repo { repo, .. } if !table.is_oci(&repo)
            );
    }
    claimed(table, host, rest).is_some()
}

/// The repository `/v2/<rest>` on `host` goes to, if any.
pub fn claimed(table: &crate::routes::Table, host: &str, rest: &[String]) -> Option<String> {
    let (name, _) = pkgpod_oci::protocol::parse_target(rest)?;
    table.registry(host, &name).map(|(repo, _)| repo)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use http::StatusCode;
    use pkgpod_base::Digest;
    use pkgpod_engine::RepoInfo;

    use super::*;

    /// An image manifest whose config names `c`.
    fn manifest(c: char) -> Vec<u8> {
        format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:{}","size":2}},"layers":[]}}"#,
            pkgpod_oci::OCI_MANIFEST,
            c.to_string().repeat(64)
        )
        .into_bytes()
    }

    /// A stand-in registry with image `x/app`, whose tag 1.0 names the
    /// manifest `answer` gives for the `n`th request (from 1), after its
    /// delay. Also how many were made.
    fn registry(
        answer: impl Fn(usize) -> (u64, char) + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<usize>>>) {
        use crate::serving::proxy::tests::{reply, stub};
        let asked = Arc::new(Mutex::new(Vec::new()));
        let asked2 = asked.clone();
        let up = stub(move |n, path, c| {
            if path != "/v2/x/app/manifests/1.0" {
                return reply(c, "404 Not Found", b"");
            }
            asked2.lock().unwrap().push(n);
            let (delay, m) = answer(n);
            std::thread::sleep(std::time::Duration::from_secs(delay));
            let head = format!("200 OK\r\nContent-Type: {}", pkgpod_oci::OCI_MANIFEST);
            reply(c, &head, &manifest(m));
        });
        (format!("{up}x"), asked)
    }

    /// Tag 1.0 of proxy `r`'s image app, as it is served to an anonymous
    /// caller: the digest.
    async fn tag(app: &Arc<crate::App>, r: &RepoInfo) -> String {
        let path = format!("/v2/{}/app/manifests/1.0", r.name);
        let rest: Vec<String> = path[4..].split('/').map(String::from).collect();
        let req = http::Request::get(path)
            .body(pkgpod_proto::body::empty())
            .unwrap();
        let caller = crate::protocols::who(app, &crate::auth::Caller::anonymous());
        let names = app.routes().unwrap();
        let resp = handle(app.clone(), caller, "localhost", names, &rest, req)
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.headers()["docker-content-digest"]
            .to_str()
            .unwrap()
            .to_string()
    }

    fn digest(m: &[u8]) -> String {
        format!("sha256:{}", Digest::of(m).hex())
    }

    /// Requests for a tag while it is fetched again share that fetch:
    /// each would hold a thread and a connection while the registry is
    /// slow, for as long as its client lets it.
    #[tokio::test(flavor = "multi_thread")]
    async fn requests_for_a_tag_share_one_fetch() {
        use crate::serving::proxy::tests::{app, proxy_repo};
        let (_d, app) = app();
        let (up, asked) = registry(|n| if n == 1 { (0, '0') } else { (8, '1') });
        let r = proxy_repo(&app, "oci-shared", "oci", &up);
        assert_eq!(tag(&app, &r).await, digest(&manifest('0')));
        let all: Vec<_> = (0..10)
            .map(|_| {
                let (app, r) = (app.clone(), r.clone());
                tokio::spawn(async move { tag(&app, &r).await })
            })
            .collect();
        for a in all {
            assert_eq!(a.await.unwrap(), digest(&manifest('0')));
        }
        assert_eq!(*asked.lock().unwrap(), [1, 2]);
    }

    /// A request that goes away while its tag is fetched leaves the fetch
    /// to be kept; one made meanwhile waits for it rather than ask itself,
    /// so what it would be sent is not replaced by the earlier answer when
    /// that comes (later, and older).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tag_fetched_for_no_one_does_not_replace_a_newer_one() {
        use crate::serving::proxy::tests::{app, proxy_repo};
        use std::time::Duration;
        let (_d, app) = app();
        let (up, asked) = registry(|n| match n {
            1 => (0, '0'),
            2 => (10, '1'),
            _ => (0, '2'),
        });
        let r = proxy_repo(&app, "oci-gone", "oci", &up);
        tag(&app, &r).await;
        // Its client gives up at 2 s.
        assert!(
            tokio::time::timeout(Duration::from_secs(2), tag(&app, &r))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(tag(&app, &r).await, digest(&manifest('0')));
        let t = std::time::Instant::now();
        while app.oci.fetching(&r.name) {
            assert!(t.elapsed() < Duration::from_secs(20), "the fetch goes on");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (app2, name) = (app.clone(), r.name.clone());
        let kept =
            tokio::task::spawn_blocking(move || app2.engine.cache_get(&name, "app/manifests/1.0"))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert_eq!(*asked.lock().unwrap(), [1, 2]);
        assert_eq!(
            format!("sha256:{}", kept.sha256.unwrap().hex()),
            digest(&manifest('1'))
        );
    }
}
