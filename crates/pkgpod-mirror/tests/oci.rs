// SPDX-License-Identifier: AGPL-3.0-only

//! The registry client against a fake registry that, like Docker Hub,
//! answers 401 with a Bearer challenge and hands out tokens at its realm.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use pkgpod_mirror::oci::{Registry, parse_ref};
use sha2::Digest as _;

fn sha(b: &[u8]) -> String {
    format!(
        "sha256:{}",
        pkgpod_base::digest::hex(&sha2::Sha256::digest(b))
    )
}

/// Serves `files` (path -> (type, body)) to requests with the token;
/// returns its base URL and the requests seen.
fn fake(
    files: Vec<(String, (String, Vec<u8>))>,
    creds: Option<&'static str>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (b2, s2) = (base.clone(), seen.clone());
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut r = BufReader::new(c.try_clone().unwrap());
            let mut first = String::new();
            r.read_line(&mut first).unwrap();
            let mut auth = String::new();
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h
                    .strip_prefix("authorization: ")
                    .or_else(|| h.strip_prefix("Authorization: "))
                {
                    auth = v.trim().to_string();
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            s2.lock().unwrap().push(format!("{path} [{auth}]"));
            let mut c = c;
            let send = |c: &mut std::net::TcpStream, st: &str, hdr: &str, body: &[u8]| {
                let _ = write!(
                    c,
                    "HTTP/1.1 {st}\r\n{hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(body);
            };
            if path.starts_with("/token") {
                // With credentials if the registry wants them (Basic).
                if let Some(want) = creds
                    && auth != format!("Basic {want}")
                {
                    send(&mut c, "401 Unauthorized", "", b"");
                    continue;
                }
                send(
                    &mut c,
                    "200 OK",
                    "Content-Type: application/json\r\n",
                    br#"{"token":"T0KEN"}"#,
                );
                continue;
            }
            if auth != "Bearer T0KEN" {
                let name = path
                    .trim_start_matches("/v2/")
                    .split("/manifests/")
                    .next()
                    .unwrap()
                    .split("/blobs/")
                    .next()
                    .unwrap()
                    .to_string();
                send(
                    &mut c,
                    "401 Unauthorized",
                    &format!(
                        "WWW-Authenticate: Bearer realm=\"{b2}/token\",service=\"fake\",scope=\"repository:{name}:pull\"\r\n"
                    ),
                    b"",
                );
                continue;
            }
            match files.iter().find(|(p, _)| *p == path) {
                Some((_, (ct, body))) => {
                    send(&mut c, "200 OK", &format!("Content-Type: {ct}\r\n"), body)
                }
                None => send(&mut c, "404 Not Found", "", b""),
            }
        }
    });
    (base, seen)
}

#[test]
fn bearer_tokens_like_docker_hub() {
    let layer = b"layer bytes".to_vec();
    let config = b"{}".to_vec();
    let m = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"digest":"{}","size":2}},"layers":[{{"digest":"{}","size":11}}]}}"#,
        sha(&config),
        sha(&layer)
    );
    let mt = "application/vnd.oci.image.manifest.v1+json".to_string();
    let files = vec![
        (
            "/v2/team/app/manifests/1.0".to_string(),
            (mt.clone(), m.clone().into_bytes()),
        ),
        (
            format!("/v2/team/app/blobs/{}", sha(&layer)),
            ("application/octet-stream".into(), layer.clone()),
        ),
    ];
    let (base, seen) = fake(files, None);
    let reg = Registry::new(None).unwrap();
    let r = parse_ref(&base, "team/app:1.0").unwrap();
    let (body, got_mt, digest) = reg.manifest(&r, &r.reference).unwrap();
    assert_eq!(body, m.as_bytes());
    assert_eq!(got_mt, mt);
    assert_eq!(digest, sha(m.as_bytes()));
    let mut out = Vec::new();
    assert_eq!(reg.blob(&r, &sha(&layer), &mut out).unwrap(), 11);
    assert_eq!(out, layer);
    let seen = seen.lock().unwrap().clone();
    // One challenge, one token, then the token reused.
    assert_eq!(
        seen.iter().filter(|s| s.starts_with("/token")).count(),
        1,
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|s| s.contains("scope=repository%3Ateam%2Fapp%3Apull")),
        "{seen:?}"
    );
    // A digest that is not the body's is refused.
    let wrong = format!("sha256:{}", "0".repeat(64));
    assert!(reg.manifest(&r, &wrong).is_err());
}

#[test]
fn token_with_credentials() {
    let m = br#"{"schemaVersion":2,"manifests":[]}"#.to_vec();
    let files = vec![(
        "/v2/x/manifests/latest".to_string(),
        (
            "application/vnd.oci.image.index.v1+json".to_string(),
            m.clone(),
        ),
    )];
    // "user:pw" in Basic.
    let (base, _) = fake(files, Some("dXNlcjpwdw=="));
    let r = parse_ref(&base, "x").unwrap();
    assert!(Registry::new(None).unwrap().manifest(&r, "latest").is_err());
    let reg = Registry::new(Some(("user".into(), "pw".into()))).unwrap();
    assert_eq!(reg.manifest(&r, "latest").unwrap().0, m);
}
