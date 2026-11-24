// SPDX-License-Identifier: AGPL-3.0-only

//! Client commands: each one a call (or a plan and its apply) of the API.

use std::io::IsTerminal;

use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

use crate::Cmd;
use crate::client::{CliError, Client, Saved, enc, load_saved, save};
use crate::term;

#[derive(Args)]
pub struct Yes {
    /// Apply without asking.
    #[arg(long, short)]
    yes: bool,
}

#[derive(Subcommand)]
pub enum RepoCmd {
    /// Repositories you may read.
    List,
    /// One repository, with its settings.
    Show { name: String },
    /// Create a repository.
    Create {
        name: String,
        #[arg(long, short, default_value = "deb")]
        format: String,
        /// hosted, import or mirror.
        #[arg(long, short, default_value = "hosted")]
        kind: String,
        #[arg(long)]
        storage: Option<String>,
        /// Settings (urls=https://pkg.example.org/ppa/ public=true ...).
        #[arg(long = "set", short = 's', value_name = "KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Change settings.
    Set {
        name: String,
        #[arg(required = true, value_name = "KEY=VALUE")]
        settings: Vec<String>,
    },
    /// Delete a repository.
    Delete {
        name: String,
        /// Also delete a mirror's received files.
        #[arg(long)]
        remove_tree: bool,
        #[command(flatten)]
        yes: Yes,
    },
    /// Publish the current state again.
    Republish { name: String },
    /// Packages of the current version.
    Packages {
        name: String,
        #[arg(default_value = "")]
        query: String,
    },
    /// Settings and operations each format has.
    Formats,
}

#[derive(Subcommand)]
pub enum UserCmd {
    List,
    Add {
        name: String,
        #[arg(long)]
        admin: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Change a user (admin, disabled, password).
    Set {
        name: String,
        #[arg(long)]
        admin: Option<bool>,
        #[arg(long)]
        disabled: Option<bool>,
        #[arg(long)]
        password: bool,
    },
    Delete {
        name: String,
    },
    /// Change your own password.
    Passwd,
}

#[derive(Subcommand)]
pub enum TeamCmd {
    List,
    Add {
        name: String,
    },
    Delete {
        name: String,
    },
    /// Add members.
    Join {
        team: String,
        users: Vec<String>,
    },
    /// Remove members.
    Leave {
        team: String,
        users: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum GrantCmd {
    /// Rights on a repository.
    List {
        repo: String,
    },
    /// Give user:<name> or team:<name> a role (read, write, admin) on a
    /// repository (or `*`, every one).
    Set {
        repo: String,
        subject: String,
        role: String,
    },
    Revoke {
        repo: String,
        subject: String,
    },
}

#[derive(Subcommand)]
pub enum TokenCmd {
    List {
        #[arg(long)]
        all: bool,
    },
    /// Make a token; it is printed once.
    Create {
        name: String,
        /// Only for these repositories.
        #[arg(long, value_delimiter = ',')]
        repos: Vec<String>,
        /// May change repositories (default: read only).
        #[arg(long)]
        write: bool,
        #[arg(long)]
        expires_days: Option<i64>,
    },
    Delete {
        id: i64,
    },
}

#[derive(Subcommand)]
pub enum ListenerCmd {
    List,
    /// Add or change a listener.
    Set {
        name: String,
        addr: String,
        #[arg(long, default_value = "")]
        cert: String,
        #[arg(long, default_value = "")]
        key: String,
        #[arg(long)]
        disabled: bool,
    },
    Delete {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum KeyCmd {
    List,
    /// Add a key that is in a gpg home.
    Add {
        name: String,
        fingerprint: String,
        #[arg(long, default_value = "")]
        homedir: String,
        #[arg(long, default_value = "")]
        passphrase_file: String,
    },
    /// Make a new key in the server's gpg home.
    Generate {
        name: String,
        uid: String,
    },
    Default {
        name: String,
    },
    Delete {
        name: String,
    },
    /// Print the public key.
    Public {
        name: String,
    },
}

#[derive(Subcommand)]
pub enum StorageCmd {
    List,
    Set { name: String, path: String },
    Delete { name: String },
}

/// `key=value` pairs as a JSON object; lists comma-separated stay text
/// (the server reads them by the schema).
fn kv(pairs: &[String]) -> Result<Value, CliError> {
    let mut m = serde_json::Map::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("{p:?}: expected KEY=VALUE")))?;
        m.insert(k.to_string(), Value::String(v.to_string()));
    }
    Ok(Value::Object(m))
}

fn query(pairs: &[String]) -> Result<String, CliError> {
    let mut q = Vec::new();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("{p:?}: expected KEY=VALUE")))?;
        q.push(format!("{}={}", enc(k), enc(v)));
    }
    Ok(q.join("&"))
}

fn when(t: Option<i64>) -> String {
    t.and_then(|t| jiff::Timestamp::from_second(t).ok())
        .map(|t| {
            t.to_zoned_datetime(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "-".into())
}

fn table(rows: Vec<Vec<String>>) {
    let n = rows.iter().map(Vec::len).max().unwrap_or(0);
    let w: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |s| s.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:<w$}  ", w = w[i]));
            }
        }
        println!("{}", line.trim_end());
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        v => v.to_string(),
    }
}

struct Ctx {
    c: Client,
    json: bool,
}

impl Ctx {
    fn print(&self, v: &Value, human: impl FnOnce(&Value)) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(v).expect("json"));
        } else {
            human(v);
        }
    }

    /// Shows a plan, asks, applies.
    fn plan(&self, path: &str, body: &Value, yes: &Yes) -> Result<(), CliError> {
        if yes.yes {
            let out = self.c.post(&format!("{path}?apply=1"), body)?;
            self.print(&out, show_outcome);
            return Ok(());
        }
        let p = self.c.post(path, body)?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&p).expect("json"));
        } else {
            show_plan(&p);
        }
        let id = p["plan"].as_str().unwrap_or_default();
        if p["total_changes"].as_u64() == Some(0) {
            println!("nothing to change");
            return Ok(());
        }
        if !std::io::stdin().is_terminal() {
            println!("not applied; apply with: pkgpod apply {id} (or run again with -y)");
            return Ok(());
        }
        if term::confirm("Apply?") {
            let out = self
                .c
                .post(&format!("/api/v1/plans/{id}/apply"), &Value::Null)?;
            self.print(&out, show_outcome);
        } else {
            println!("not applied (plan {id} is kept for an hour)");
        }
        Ok(())
    }
}

fn show_changes(v: &Value) {
    for c in v["changes"].as_array().into_iter().flatten() {
        let mark = match c["change"].as_str() {
            Some("added") => "+",
            Some("removed") => "-",
            _ => "~",
        };
        println!("  {mark} {}", s(&c["path"]));
    }
    let total = v["total_changes"].as_u64().unwrap_or(0);
    let shown = v["changes"].as_array().map_or(0, Vec::len) as u64;
    if total > shown {
        println!("  ... and {} more", total - shown);
    }
}

fn show_plan(v: &Value) {
    println!("{} (from version {})", s(&v["summary"]), s(&v["base"]));
    show_changes(v);
}

fn show_outcome(v: &Value) {
    match v["version"].as_i64() {
        Some(n) => println!("{}: version {n}", s(&v["summary"])),
        None => println!("{}: nothing changed", s(&v["summary"])),
    }
}

pub fn run(
    url: Option<String>,
    token: Option<String>,
    json: bool,
    cmd: Cmd,
) -> Result<(), CliError> {
    let saved = load_saved();
    let url = url
        .or_else(|| saved.as_ref().map(|s| s.url.clone()))
        .unwrap_or_else(|| "http://127.0.0.1:8090".into());
    if let Cmd::Login {
        user,
        password_stdin,
    } = &cmd
    {
        return login(&url, user, *password_stdin);
    }
    let token = token
        .or_else(|| saved.map(|s| s.token))
        .filter(|t| !t.is_empty());
    let x = Ctx {
        c: Client::new(&url, token.map(|t| (t, None)))?,
        json,
    };
    let c = &x.c;
    match cmd {
        Cmd::Serve(_) | Cmd::Admin { .. } | Cmd::Login { .. } => unreachable!("handled before"),
        Cmd::Status => {
            let v = c.get("/api/v1/status")?;
            x.print(&v, |v| {
                println!("server {} at {}", s(&v["pkgpod"]), c.base);
                match v["user"].as_str() {
                    Some(u) => println!(
                        "logged in as {u}{}",
                        if v["admin"] == json!(true) {
                            " (admin)"
                        } else {
                            ""
                        }
                    ),
                    None => println!("not logged in"),
                }
            });
        }
        Cmd::Repo(r) => repo(&x, r)?,
        Cmd::Upload {
            repo,
            files,
            params,
        } => {
            let q = query(&params)?;
            for f in files {
                let fname = f
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let sep = if q.is_empty() { "" } else { "&" };
                let path = format!(
                    "/api/v1/repos/{}/upload?{q}{sep}filename={}",
                    enc(&repo),
                    enc(&fname)
                );
                let v = c.upload(&path, &f)?;
                x.print(&v, |v| {
                    for r in v.as_array().into_iter().flatten() {
                        show_outcome(r);
                    }
                });
            }
        }
        Cmd::Op {
            repo,
            op,
            params,
            yes,
        } => {
            x.plan(
                &format!("/api/v1/repos/{}/ops/{}", enc(&repo), enc(&op)),
                &kv(&params)?,
                &yes,
            )?;
        }
        Cmd::Ls {
            repo,
            path,
            version,
            served,
        } => {
            let mut q = format!("path={}", enc(&path));
            if let Some(v) = version {
                q.push_str(&format!("&version={}", enc(&v)));
            }
            if served {
                q.push_str("&served=1");
            }
            let v = c.get(&format!("/api/v1/repos/{}/tree?{q}", enc(&repo)))?;
            x.print(&v, |v| {
                let mut rows = Vec::new();
                for e in v["entries"].as_array().into_iter().flatten() {
                    let name = s(&e["name"]);
                    rows.push(match e["type"].as_str() {
                        Some("dir") => vec![format!("{name}/"), String::new()],
                        Some("file") => vec![name, s(&e["size"])],
                        Some("value") => vec![name, e["value"].to_string()],
                        _ => vec![name, s(&e["target"])],
                    });
                }
                table(rows);
            });
        }
        Cmd::Versions { repo } => {
            let v = c.get(&format!("/api/v1/repos/{}/versions", enc(&repo)))?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "VERSION".into(),
                    "WHEN".into(),
                    "BY".into(),
                    "LABELS".into(),
                    "SUMMARY".into(),
                ]];
                for e in v.as_array().into_iter().flatten() {
                    let labels: Vec<String> = e["labels"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(s)
                        .collect();
                    rows.push(vec![
                        s(&e["seq"]),
                        when(e["created"].as_i64()),
                        s(&e["actor"]),
                        labels.join(" "),
                        s(&e["summary"]),
                    ]);
                }
                table(rows);
            });
        }
        Cmd::Diff { repo, from, to } => {
            let mut q = format!("from={}", enc(&from));
            if let Some(t) = to {
                q.push_str(&format!("&to={}", enc(&t)));
            }
            let v = c.get(&format!("/api/v1/repos/{}/diff?{q}", enc(&repo)))?;
            x.print(&v, |v| {
                println!("version {} -> {}", s(&v["from"]), s(&v["to"]));
                show_changes(v);
            });
        }
        Cmd::Label {
            repo,
            name,
            version,
            note,
            force,
        } => {
            let v = c.post(
                &format!("/api/v1/repos/{}/labels", enc(&repo)),
                &json!({"name": name, "version": version, "note": note, "force": force}),
            )?;
            x.print(&v, |v| println!("{name} -> version {}", s(&v["seq"])));
        }
        Cmd::Unlabel { repo, name } => {
            c.send(
                Method::DELETE,
                &format!("/api/v1/repos/{}/labels/{}", enc(&repo), enc(&name)),
                &Value::Null,
            )?;
        }
        Cmd::Rollback { repo, version, yes } => {
            x.plan(
                &format!("/api/v1/repos/{}/rollback", enc(&repo)),
                &json!({"version": version}),
                &yes,
            )?;
        }
        Cmd::Promote {
            from,
            to,
            paths,
            version,
            dist,
            components,
            architectures,
            into,
            component,
            replace,
            yes,
        } => {
            let body = match dist {
                Some(dist) => json!({
                    "from": from, "dist": dist, "packages": paths,
                    "components": components, "architectures": architectures,
                    "into": into, "component": component, "replace": replace,
                }),
                None => json!({"from": from, "version": version, "paths": paths}),
            };
            x.plan(&format!("/api/v1/repos/{}/promote", enc(&to)), &body, &yes)?;
        }
        Cmd::Rm { repo, paths, yes } => {
            x.plan(
                &format!("/api/v1/repos/{}/remove", enc(&repo)),
                &json!({"paths": paths}),
                &yes,
            )?;
        }
        Cmd::Search { query, repo } => {
            let mut q = format!("q={}", enc(&query));
            if let Some(r) = repo {
                q.push_str(&format!("&repo={}", enc(&r)));
            }
            let v = c.get(&format!("/api/v1/search?{q}"))?;
            x.print(&v, print_packages);
        }
        Cmd::Apply { plan } => {
            let v = c.post(&format!("/api/v1/plans/{}/apply", enc(&plan)), &Value::Null)?;
            x.print(&v, show_outcome);
        }
        Cmd::User(u) => user(&x, u)?,
        Cmd::Team(t) => match t {
            TeamCmd::List => {
                let v = c.get("/api/v1/teams")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|t| {
                                let m: Vec<String> = t["members"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .map(s)
                                    .collect();
                                vec![s(&t["name"]), m.join(" ")]
                            })
                            .collect(),
                    );
                });
            }
            TeamCmd::Add { name } => {
                c.post("/api/v1/teams", &json!({"name": name}))?;
            }
            TeamCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/teams/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
            TeamCmd::Join { team, users } => {
                for u in users {
                    c.send(
                        Method::PUT,
                        &format!("/api/v1/teams/{}/members/{}", enc(&team), enc(&u)),
                        &Value::Null,
                    )?;
                }
            }
            TeamCmd::Leave { team, users } => {
                for u in users {
                    c.send(
                        Method::DELETE,
                        &format!("/api/v1/teams/{}/members/{}", enc(&team), enc(&u)),
                        &Value::Null,
                    )?;
                }
            }
        },
        Cmd::Grant(g) => match g {
            GrantCmd::List { repo } => {
                let v = c.get(&format!("/api/v1/repos/{}/grants", enc(&repo)))?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|g| vec![s(&g["subject"]), s(&g["role"])])
                            .collect(),
                    );
                });
            }
            GrantCmd::Set {
                repo,
                subject,
                role,
            } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/repos/{}/grants", enc(&repo)),
                    &json!({"subject": subject, "role": role}),
                )?;
            }
            GrantCmd::Revoke { repo, subject } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/repos/{}/grants/{}", enc(&repo), enc(&subject)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Token(t) => match t {
            TokenCmd::List { all } => {
                let v = c.get(if all {
                    "/api/v1/tokens?all=1"
                } else {
                    "/api/v1/tokens"
                })?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "ID".into(),
                        "USER".into(),
                        "NAME".into(),
                        "REPOS".into(),
                        "WRITE".into(),
                        "EXPIRES".into(),
                        "USED".into(),
                    ]];
                    for t in v.as_array().into_iter().flatten() {
                        let repos: Vec<String> =
                            t["repos"].as_array().into_iter().flatten().map(s).collect();
                        rows.push(vec![
                            s(&t["id"]),
                            s(&t["user"]),
                            s(&t["name"]),
                            if repos.is_empty() {
                                "*".into()
                            } else {
                                repos.join(",")
                            },
                            s(&t["write"]),
                            when(t["expires"].as_i64()),
                            when(t["used"].as_i64()),
                        ]);
                    }
                    table(rows);
                });
            }
            TokenCmd::Create {
                name,
                repos,
                write,
                expires_days,
            } => {
                let v = c.post(
                    "/api/v1/tokens",
                    &json!({"name": name, "repos": repos, "write": write, "expires_days": expires_days}),
                )?;
                x.print(&v, |v| println!("{}", s(&v["token"])));
            }
            TokenCmd::Delete { id } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/tokens/{id}"),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Listener(l) => match l {
            ListenerCmd::List => {
                let v = c.get("/api/v1/listeners")?;
                x.print(&v, |v| {
                    let mut rows = vec![vec![
                        "NAME".into(),
                        "ADDRESS".into(),
                        "TLS".into(),
                        "STATE".into(),
                    ]];
                    for l in v.as_array().into_iter().flatten() {
                        let state = if l["running"] == json!(true) {
                            "running".to_string()
                        } else if let Some(e) = l["error"].as_str() {
                            format!("failed: {e}")
                        } else if l["enabled"] == json!(false) {
                            "disabled".into()
                        } else {
                            "stopped".into()
                        };
                        rows.push(vec![
                            s(&l["name"]),
                            s(&l["addr"]),
                            if l["cert"] == json!("") {
                                "no".into()
                            } else {
                                "yes".into()
                            },
                            state,
                        ]);
                    }
                    table(rows);
                });
            }
            ListenerCmd::Set {
                name,
                addr,
                cert,
                key,
                disabled,
            } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/listeners/{}", enc(&name)),
                    &json!({"addr": addr, "cert": cert, "key": key, "enabled": !disabled}),
                )?;
            }
            ListenerCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/listeners/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Key(k) => match k {
            KeyCmd::List => {
                let v = c.get("/api/v1/keys")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|k| {
                                vec![
                                    s(&k["name"]),
                                    s(&k["fingerprint"]),
                                    if k["default"] == json!(true) {
                                        "default".into()
                                    } else {
                                        String::new()
                                    },
                                ]
                            })
                            .collect(),
                    );
                });
            }
            KeyCmd::Add {
                name,
                fingerprint,
                homedir,
                passphrase_file,
            } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/keys/{}", enc(&name)),
                    &json!({"fingerprint": fingerprint, "homedir": homedir, "passphrase_file": passphrase_file}),
                )?;
            }
            KeyCmd::Generate { name, uid } => {
                let v = c.post(
                    &format!("/api/v1/keys/{}/generate", enc(&name)),
                    &json!({"uid": uid}),
                )?;
                x.print(&v, |v| {
                    println!("{} {}", s(&v["name"]), s(&v["fingerprint"]))
                });
            }
            KeyCmd::Default { name } => {
                c.post(
                    &format!("/api/v1/keys/{}/default", enc(&name)),
                    &Value::Null,
                )?;
            }
            KeyCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/keys/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
            KeyCmd::Public { name } => {
                print!(
                    "{}",
                    c.get_text(&format!("/api/v1/keys/{}/public", enc(&name)))?
                );
            }
        },
        Cmd::Storage(st) => match st {
            StorageCmd::List => {
                let v = c.get("/api/v1/storages")?;
                x.print(&v, |v| {
                    table(
                        v.as_array()
                            .into_iter()
                            .flatten()
                            .map(|s2| vec![s(&s2["name"]), s(&s2["path"])])
                            .collect(),
                    )
                });
            }
            StorageCmd::Set { name, path } => {
                c.send(
                    Method::PUT,
                    &format!("/api/v1/storages/{}", enc(&name)),
                    &json!({"path": path}),
                )?;
            }
            StorageCmd::Delete { name } => {
                c.send(
                    Method::DELETE,
                    &format!("/api/v1/storages/{}", enc(&name)),
                    &Value::Null,
                )?;
            }
        },
        Cmd::Sync { repo, wait } => {
            let v = c.post(&format!("/api/v1/repos/{}/sync", enc(&repo)), &Value::Null)?;
            follow(&x, &v, wait)?;
        }
        Cmd::Import {
            repo,
            source,
            dists,
            components,
            architectures,
            keyring,
            replace,
            wait,
        } => {
            let keyring = match keyring {
                Some(p) => std::fs::read_to_string(&p)
                    .map_err(|e| CliError::usage(format!("{}: {e}", p.display())))?,
                None => String::new(),
            };
            let v = c.post(
                &format!("/api/v1/repos/{}/import", enc(&repo)),
                &json!({"source": source, "dists": dists, "components": components,
                        "architectures": architectures, "keyring": keyring, "replace": replace}),
            )?;
            follow(&x, &v, wait)?;
        }
        Cmd::Jobs { repo } => {
            let v = match repo {
                Some(r) => c.get(&format!("/api/v1/repos/{}/jobs", enc(&r)))?,
                None => c.get("/api/v1/jobs")?,
            };
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "JOB".into(),
                    "REPO".into(),
                    "STATE".into(),
                    "STARTED".into(),
                    "FINISHED".into(),
                    "SUMMARY".into(),
                ]];
                for j in v.as_array().into_iter().flatten() {
                    rows.push(vec![
                        s(&j["id"]),
                        s(&j["repo"]),
                        s(&j["state"]),
                        when(j["started"].as_i64()),
                        when(j["finished"].as_i64()),
                        s(&j["summary"]),
                    ]);
                }
                table(rows);
            });
        }
        Cmd::Job { id, cancel } => {
            if cancel {
                c.post(&format!("/api/v1/jobs/{id}/cancel"), &Value::Null)?;
                println!("asked job {id} to stop");
                return Ok(());
            }
            let j = c.get(&format!("/api/v1/jobs/{id}"))?;
            x.print(&j, |j| {
                println!(
                    "job {} ({} of {}): {} {}",
                    s(&j["id"]),
                    s(&j["kind"]),
                    s(&j["repo"]),
                    s(&j["state"]),
                    s(&j["summary"])
                );
                print!("{}", j["log"].as_str().unwrap_or(""));
            });
        }
        Cmd::Gc {
            dry_run,
            grace_hours,
        } => {
            let v = c.post(
                "/api/v1/gc",
                &json!({"dry_run": dry_run, "grace_hours": grace_hours}),
            )?;
            x.print(&v, |v| {
                println!(
                    "{}{} versions, {} tree objects, {} blobs ({} bytes)",
                    if dry_run { "would remove " } else { "removed " },
                    s(&v["versions"]),
                    s(&v["objects"]),
                    s(&v["blobs"]),
                    s(&v["bytes"])
                )
            });
        }
    }
    Ok(())
}

/// Prints a started job, or waits for it and prints its log.
fn follow(x: &Ctx, v: &Value, wait: bool) -> Result<(), CliError> {
    let id = v["job"].as_i64().unwrap_or(0);
    if !wait {
        x.print(v, |_| {
            println!("job {id} started; follow it with: pkgpod job {id}")
        });
        return Ok(());
    }
    let mut shown = 0;
    loop {
        let j = x.c.get(&format!("/api/v1/jobs/{id}"))?;
        let log = j["log"].as_str().unwrap_or("");
        if log.len() > shown {
            print!("{}", &log[shown..]);
            shown = log.len();
        }
        match j["state"].as_str() {
            Some("running" | "queued") => std::thread::sleep(std::time::Duration::from_secs(1)),
            Some("ok") => return Ok(()),
            _ => return Err(CliError::other(format!("job {id}: {}", s(&j["summary"])))),
        }
    }
}

fn print_packages(v: &Value) {
    let mut rows = vec![vec![
        "REPO".into(),
        "NAME".into(),
        "VERSION".into(),
        "WHERE".into(),
    ]];
    for h in v.as_array().into_iter().flatten() {
        rows.push(vec![
            s(&h["repo"]),
            s(&h["name"]),
            s(&h["version"]),
            s(&h["extra"]),
        ]);
    }
    table(rows);
}

fn repo(x: &Ctx, cmd: RepoCmd) -> Result<(), CliError> {
    let c = &x.c;
    match cmd {
        RepoCmd::List => {
            let v = c.get("/api/v1/repos")?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "NAME".into(),
                    "FORMAT".into(),
                    "KIND".into(),
                    "VERSION".into(),
                    "UPDATED".into(),
                    "ADDRESSES".into(),
                ]];
                for r in v.as_array().into_iter().flatten() {
                    let urls: Vec<String> = r["settings"]["urls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(s)
                        .collect();
                    rows.push(vec![
                        s(&r["name"]),
                        s(&r["format"]),
                        s(&r["kind"]),
                        s(&r["version"]),
                        when(r["updated"].as_i64()),
                        if urls.is_empty() {
                            format!("/{}/", s(&r["name"]))
                        } else {
                            urls.join(" ")
                        },
                    ]);
                }
                table(rows);
            });
        }
        RepoCmd::Show { name } => {
            let v = c.get(&format!("/api/v1/repos/{}", enc(&name)))?;
            x.print(&v, |v| {
                println!(
                    "{} ({} {}), storage {}, version {}",
                    s(&v["name"]),
                    s(&v["format"]),
                    s(&v["kind"]),
                    s(&v["storage"]),
                    s(&v["version"])
                );
                if let Some(m) = v["settings"].as_object() {
                    for (k, val) in m {
                        println!("  {k} = {val}");
                    }
                }
            });
        }
        RepoCmd::Create {
            name,
            format,
            kind,
            storage,
            settings,
        } => {
            let v = c.post(
                "/api/v1/repos",
                &json!({"name": name, "format": format, "kind": kind, "storage": storage, "settings": kv(&settings)?}),
            )?;
            x.print(&v, |v| {
                println!(
                    "created {} ({} {})",
                    s(&v["name"]),
                    s(&v["format"]),
                    s(&v["kind"])
                )
            });
        }
        RepoCmd::Set { name, settings } => {
            let v = c.send(
                Method::PATCH,
                &format!("/api/v1/repos/{}", enc(&name)),
                &json!({"settings": kv(&settings)?}),
            )?;
            x.print(&v, |_| println!("settings of {name} changed"));
        }
        RepoCmd::Delete {
            name,
            remove_tree,
            yes,
        } => {
            if !yes.yes && !term::confirm(&format!("Delete {name} with all its versions?")) {
                return Err(CliError::usage("not deleted (use -y to skip the question)"));
            }
            c.send(
                Method::DELETE,
                &format!(
                    "/api/v1/repos/{}{}",
                    enc(&name),
                    if remove_tree { "?remove_tree=1" } else { "" }
                ),
                &Value::Null,
            )?;
            println!("deleted {name}");
        }
        RepoCmd::Republish { name } => {
            let v = c.post(
                &format!("/api/v1/repos/{}/republish", enc(&name)),
                &Value::Null,
            )?;
            x.print(&v, |v| println!("version {}", s(&v["version"])));
        }
        RepoCmd::Packages { name, query } => {
            let v = c.get(&format!(
                "/api/v1/repos/{}/packages?q={}",
                enc(&name),
                enc(&query)
            ))?;
            x.print(&v, print_packages);
        }
        RepoCmd::Formats => {
            let v = c.get("/api/v1/formats")?;
            x.print(&v, |v| {
                for f in v.as_array().into_iter().flatten() {
                    let kinds: Vec<String> =
                        f["kinds"].as_array().into_iter().flatten().map(s).collect();
                    println!("{} ({}): {}", s(&f["id"]), s(&f["title"]), kinds.join(", "));
                    println!("  settings:");
                    for st in f["settings"].as_array().into_iter().flatten() {
                        println!(
                            "    {:<16} {:<8} {}",
                            s(&st["name"]),
                            s(&st["type"]),
                            s(&st["help"])
                        );
                    }
                    println!("  operations:");
                    for o in f["ops"].as_array().into_iter().flatten() {
                        let ps: Vec<String> = o["params"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|p| s(&p["name"]))
                            .collect();
                        println!(
                            "    {:<12} {} ({})",
                            s(&o["name"]),
                            s(&o["help"]),
                            ps.join(", ")
                        );
                    }
                }
            });
        }
    }
    Ok(())
}

fn user(x: &Ctx, cmd: UserCmd) -> Result<(), CliError> {
    let c = &x.c;
    let read_pw = |stdin: bool, prompt: &str| {
        if stdin {
            term::read_stdin_line()
        } else {
            term::read_password(prompt)
        }
        .map_err(|e| CliError::other(format!("reading the password: {e}")))
    };
    match cmd {
        UserCmd::List => {
            let v = c.get("/api/v1/users")?;
            x.print(&v, |v| {
                let mut rows = vec![vec![
                    "NAME".into(),
                    "ADMIN".into(),
                    "DISABLED".into(),
                    "CREATED".into(),
                ]];
                for u in v.as_array().into_iter().flatten() {
                    rows.push(vec![
                        s(&u["name"]),
                        s(&u["admin"]),
                        s(&u["disabled"]),
                        when(u["created"].as_i64()),
                    ]);
                }
                table(rows);
            });
        }
        UserCmd::Add {
            name,
            admin,
            password_stdin,
        } => {
            let pw = read_pw(password_stdin, &format!("Password for {name}: "))?;
            c.post(
                "/api/v1/users",
                &json!({"name": name, "password": pw, "admin": admin}),
            )?;
            println!("user {name} created");
        }
        UserCmd::Set {
            name,
            admin,
            disabled,
            password,
        } => {
            let pw = if password {
                Some(read_pw(false, &format!("New password for {name}: "))?)
            } else {
                None
            };
            c.send(
                Method::PATCH,
                &format!("/api/v1/users/{}", enc(&name)),
                &json!({"admin": admin, "disabled": disabled, "password": pw}),
            )?;
        }
        UserCmd::Delete { name } => {
            c.send(
                Method::DELETE,
                &format!("/api/v1/users/{}", enc(&name)),
                &Value::Null,
            )?;
        }
        UserCmd::Passwd => {
            let old = read_pw(false, "Current password: ")?;
            let new = read_pw(false, "New password: ")?;
            c.post("/api/v1/me/password", &json!({"old": old, "new": new}))?;
            println!("password changed");
        }
    }
    Ok(())
}

fn login(url: &str, user: &str, stdin: bool) -> Result<(), CliError> {
    let pw = if stdin {
        term::read_stdin_line()
    } else {
        term::read_password(&format!("Password for {user} at {url}: "))
    }
    .map_err(|e| CliError::other(format!("reading the password: {e}")))?;
    let c = Client::new(url, Some((user.to_string(), Some(pw))))?;
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let v = c.post(
        "/api/v1/tokens",
        &json!({"name": format!("cli {}", host.trim()), "write": true}),
    )?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| CliError::other("no token in the reply"))?;
    let p = save(&Saved {
        url: url.to_string(),
        token: token.to_string(),
    })?;
    println!("logged in as {user}; token saved in {}", p.display());
    Ok(())
}
