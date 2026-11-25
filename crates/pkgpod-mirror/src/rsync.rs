// SPDX-License-Identifier: AGPL-3.0-only

//! Verbatim mirrors over rsync (daemon sources only).
//!
//! `apt` mode is the two-pass sync Debian mirrors use: pass 1 copies
//! everything but the indices (new pool files land before any index refers
//! to them), pass 2 copies the indices and deletes what upstream dropped,
//! then a `project/trace/<host>` file is written. `tree` mode is one pass
//! with deletes, for plain trees such as cloud images.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use crate::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Apt,
    /// dnf/yum repositories: packages first, then each `repodata/`.
    Rpm,
    /// Arch Linux repositories: packages first, then the databases.
    Arch,
    /// Alpine repositories: packages first, then each APKINDEX.
    Apk,
    Tree,
}

#[derive(Debug, Clone)]
pub struct RsyncMirror {
    /// `rsync://host/module/path/` or `host::module/path/`.
    pub source: String,
    pub mode: Mode,
    pub rsync: PathBuf,
    /// Delete what upstream dropped (otherwise only add and update).
    pub delete: bool,
    /// rsync stops deleting beyond this many files (exit 25).
    pub max_delete: usize,
    pub hard_links: bool,
    pub connect_timeout: u32,
    pub io_timeout: u32,
    /// Bandwidth limit in KiB/s (`--bwlimit`).
    pub bwlimit: Option<u32>,
    pub excludes: Vec<String>,
    /// Writes `project/trace/<host>` after an `apt` sync.
    pub trace_host: Option<String>,
}

impl RsyncMirror {
    pub fn new(source: &str, mode: Mode) -> Self {
        Self {
            source: source.into(),
            mode,
            rsync: "rsync".into(),
            delete: true,
            max_delete: 10_000,
            hard_links: true,
            connect_timeout: 30,
            io_timeout: 900,
            bwlimit: None,
            excludes: vec![],
            trace_host: None,
        }
    }

    /// Only rsync daemon sources: a `host:path` source would run a remote
    /// shell, and a leading `-` would be an option.
    pub fn validate(&self) -> Result<(), Error> {
        let s = &self.source;
        let daemon = s.starts_with("rsync://") || (s.contains("::") && !s.starts_with('-'));
        if !daemon || s.contains(char::is_whitespace) {
            return Err(Error::Config(format!(
                "{s:?} is not an rsync daemon source (rsync://host/module/ or host::module)"
            )));
        }
        if let Some(h) = &self.trace_host
            && (h.is_empty() || h.contains('/') || h.starts_with('.'))
        {
            return Err(Error::Config(format!("invalid trace host {h:?}")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Pass {
    pub stage: String,
    pub exit_code: i32,
    pub files_transferred: u64,
    pub bytes_transferred: u64,
    pub files_deleted: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RsyncReport {
    pub passes: Vec<Pass>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// rsync stopped deleting at max_delete.
    pub brake: bool,
    pub warnings: Vec<String>,
}

pub struct Ctx<'a> {
    pub dest: &'a Path,
    pub cancel: &'a AtomicBool,
    pub log: &'a (dyn Fn(&str) + Sync),
}

const INDEX_EXCLUDES: [&str; 4] = ["Packages*", "Sources*", "Release*", "InRelease"];
/// A dnf repository's metadata: it names the packages, so it comes last.
const RPM_INDEX_EXCLUDES: [&str; 1] = ["repodata/"];
/// An Alpine repository's index: last.
const APK_INDEX_EXCLUDES: [&str; 1] = ["APKINDEX.tar.gz"];
/// An Arch repository's databases (and their signatures): last.
const ARCH_INDEX_EXCLUDES: [&str; 7] = [
    "*.db",
    "*.db.sig",
    "*.db.tar.*",
    "*.files",
    "*.files.sig",
    "*.files.tar.*",
    "*.links.tar.*",
];
const TEMP_EXCLUDES: [&str; 2] = [".~tmp~", ".*.??????"];

fn exit_meaning(code: i32) -> &'static str {
    match code {
        1 => "syntax or usage error",
        2 => "protocol incompatibility",
        3 => "errors selecting input/output files",
        5 => "error starting the client-server protocol",
        10 => "error in socket I/O",
        11 => "error in file I/O",
        12 => "error in the rsync protocol data stream",
        20 => "killed by a signal",
        23 => "partial transfer due to error",
        25 => "stopped deleting at max_delete",
        30 => "timeout in data send/receive",
        35 => "timeout waiting for daemon connection",
        _ => "error",
    }
}

/// Mirrors `m` into `ctx.dest`. `Err` for configuration problems and
/// interruption; a failed rsync run is reported.
pub fn sync(m: &RsyncMirror, ctx: &Ctx) -> Result<RsyncReport, Error> {
    m.validate()?;
    std::fs::create_dir_all(ctx.dest)?;
    remove_update_markers(ctx.dest, ctx.log);
    let mut report = RsyncReport::default();
    let temp: Vec<String> = TEMP_EXCLUDES
        .iter()
        .map(|e| format!("--exclude={e}"))
        .collect();
    let passes: Vec<(&str, Vec<String>, bool)> = match m.mode {
        Mode::Tree => vec![("tree", temp.clone(), m.delete)],
        Mode::Apt | Mode::Rpm | Mode::Arch | Mode::Apk => {
            let excludes: &[&str] = match m.mode {
                Mode::Rpm => &RPM_INDEX_EXCLUDES,
                Mode::Arch => &ARCH_INDEX_EXCLUDES,
                Mode::Apk => &APK_INDEX_EXCLUDES,
                _ => &INDEX_EXCLUDES,
            };
            let mut first: Vec<String> =
                excludes.iter().map(|e| format!("--exclude={e}")).collect();
            first.extend(temp.clone());
            let mut second = temp.clone();
            if let Some(h) = &m.trace_host {
                second.push(format!("--exclude=project/trace/{h}"));
            }
            vec![("payloads", first, false), ("indexes", second, m.delete)]
        }
    };
    for (stage, extra, delete) in passes {
        (ctx.log)(&format!(
            "rsync {stage}: {} -> {}",
            m.source,
            ctx.dest.display()
        ));
        let pass = run_pass(m, ctx, stage, &extra, delete)?;
        let code = pass.exit_code;
        report.passes.push(pass);
        // Pool files on disk before the indices that name them arrive.
        if matches!(code, 0 | 24) && stage == "payloads" {
            crate::http::sync_fs(ctx.dest)?;
        }
        match code {
            0 => {}
            24 => report.warnings.push(format!(
                "{stage}: some source files vanished during the transfer"
            )),
            25 => {
                report.brake = true;
                report.error = Some(format!(
                    "{stage}: rsync stopped deleting at max_delete ({}); the tree may hold files upstream dropped",
                    m.max_delete
                ));
                return Ok(report);
            }
            c => {
                report.error = Some(format!("{stage}: rsync exited {c} ({})", exit_meaning(c)));
                return Ok(report);
            }
        }
    }
    if m.mode == Mode::Apt
        && let Some(h) = &m.trace_host
    {
        let trace = ctx.dest.join("project/trace").join(h);
        let now = time::OffsetDateTime::now_utc();
        let fmt = time::macros::format_description!(
            "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
        );
        // A new file renamed into place: never rewrite the old inode (it
        // may be shared, e.g. a hard link of another tree) and never leave
        // a half-written trace.
        let at = |e: std::io::Error| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", trace.display()),
            ))
        };
        std::fs::create_dir_all(trace.parent().expect("has parent")).map_err(at)?;
        let tmp = trace.with_file_name(format!(".{h}.pkgpod-tmp-{}", std::process::id()));
        std::fs::write(&tmp, format!("{}\n", now.format(fmt).expect("formats")))
            .and_then(|()| {
                std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))
            })
            .and_then(|()| std::fs::rename(&tmp, &trace))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                at(e)
            })?;
    }
    remove_update_markers(ctx.dest, ctx.log);
    report.ok = true;
    Ok(report)
}

fn remove_update_markers(dest: &Path, log: &(dyn Fn(&str) + Sync)) {
    let Ok(rd) = std::fs::read_dir(dest) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("Archive-Update-in-Progress-")
            && e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink())
            && std::fs::remove_file(e.path()).is_ok()
        {
            log(&format!("removed upstream update marker {name}"));
        }
    }
}

fn run_pass(
    m: &RsyncMirror,
    ctx: &Ctx,
    stage: &str,
    extra: &[String],
    delete: bool,
) -> Result<Pass, Error> {
    let mut cmd = Command::new(&m.rsync);
    cmd.args([
        "--recursive",
        "--times",
        "--links",
        // Links that point out of the tree (absolute, or up through ..)
        // are not made: the tree is served.
        "--safe-links",
        "--no-perms",
        "--no-owner",
        "--no-group",
        "--chmod=a+rX,u+w",
        "--partial-dir=.rsync-partial",
        "--delay-updates",
        "--no-motd",
        "--outbuf=L",
        "--itemize-changes",
        "--info=stats2",
        "--no-human-readable",
    ])
    .arg(format!("--timeout={}", m.io_timeout))
    .arg(format!("--contimeout={}", m.connect_timeout))
    .arg(format!("--max-delete={}", m.max_delete))
    .arg("--exclude=/Archive-Update-in-Progress-*");
    if m.hard_links {
        cmd.arg("--hard-links");
    }
    if let Some(bw) = m.bwlimit {
        cmd.arg(format!("--bwlimit={bw}"));
    }
    if delete {
        cmd.arg("--delete-after");
    }
    for e in &m.excludes {
        cmd.arg(format!("--exclude={e}"));
    }
    cmd.args(extra)
        .arg(&m.source)
        .arg(format!("{}/", ctx.dest.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group: rsync forks a receiver that holds the pipes
    // too, and a cancel has to stop both.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Config(format!("running {}: {e}", m.rsync.display())))?;
    // A huge sync should be the first thing the OOM killer takes, not the
    // server (best effort; raising the score needs no privilege).
    let _ = std::fs::write(format!("/proc/{}/oom_score_adj", child.id()), "500");

    let mut pass = Pass {
        stage: stage.into(),
        ..Default::default()
    };
    let stats = std::sync::Mutex::new(Vec::<String>::new());
    let code = std::thread::scope(|s| -> Result<i32, Error> {
        let pump = |r: Box<dyn Read + Send>, err: bool| {
            let stats = &stats;
            s.spawn(move || {
                for line in BufReader::new(r).lines().map_while(Result::ok) {
                    if line.starts_with("Number of ")
                        || line.starts_with("Total transferred file size")
                    {
                        stats.lock().unwrap().push(line.clone());
                    }
                    (ctx.log)(&if err { format!("rsync: {line}") } else { line });
                }
            });
        };
        pump(Box::new(child.stdout.take().expect("piped")), false);
        pump(Box::new(child.stderr.take().expect("piped")), true);
        wait(&mut child, ctx.cancel)
    })?;
    pass.exit_code = code;
    for line in stats.into_inner().unwrap() {
        let num = |l: &str| -> u64 {
            // "Number of deleted files: 2 (reg: 1, dir: 1)"
            l.split_once(':')
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|v| v.replace(',', "").parse().ok())
                .unwrap_or(0)
        };
        if line.starts_with("Number of regular files transferred") {
            pass.files_transferred = num(&line);
        } else if line.starts_with("Total transferred file size") {
            pass.bytes_transferred = num(&line);
        } else if line.starts_with("Number of deleted files") {
            pass.files_deleted = num(&line);
        }
    }
    Ok(pass)
}

/// Waits for rsync; on cancel sends its process group SIGTERM (then
/// SIGKILL) and reports the interruption. Signalling only the process
/// rsync started with leaves its receiver running with the output pipes
/// open, and the job then never ends.
fn wait(child: &mut Child, cancel: &AtomicBool) -> Result<i32, Error> {
    let pid = rustix::process::Pid::from_raw(child.id() as i32);
    let group = |sig| {
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process_group(pid, sig);
        }
    };
    loop {
        if let Some(st) = child.try_wait()? {
            return Ok(st.code().unwrap_or(20));
        }
        if cancel.load(Ordering::Relaxed) {
            group(rustix::process::Signal::TERM);
            for _ in 0..50 {
                if child.try_wait()?.is_some() {
                    // A receiver that outlived it is not waited for.
                    group(rustix::process::Signal::KILL);
                    return Err(Error::Interrupted);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            group(rustix::process::Signal::KILL);
            let _ = child.wait();
            return Err(Error::Interrupted);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Lists the source (`--list-only`) without changing anything.
pub fn probe(m: &RsyncMirror) -> Result<String, String> {
    m.validate().map_err(|e| e.to_string())?;
    let out = Command::new(&m.rsync)
        .args(["--no-motd", "--list-only"])
        .arg(format!("--contimeout={}", m.connect_timeout))
        .arg(format!("--timeout={}", m.connect_timeout.max(30)))
        .arg(&m.source)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running {}: {e}", m.rsync.display()))?;
    let code = out.status.code().unwrap_or(20);
    if code != 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "rsync exited {code} ({}): {}",
            exit_meaning(code),
            err.lines().last().unwrap_or("").trim()
        ));
    }
    let n = String::from_utf8_lossy(&out.stdout).lines().count();
    Ok(format!("reachable, {n} top-level entries"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_daemon_sources() {
        for ok in [
            "rsync://ftp.example.org/debian/",
            "ftp.example.org::debian/",
        ] {
            assert!(RsyncMirror::new(ok, Mode::Apt).validate().is_ok(), "{ok}");
        }
        for bad in [
            "ftp.example.org:/srv/debian",
            "/srv/debian",
            "-e ssh x::y",
            "rsync://a b/",
        ] {
            assert!(
                RsyncMirror::new(bad, Mode::Apt).validate().is_err(),
                "{bad}"
            );
        }
    }
}
