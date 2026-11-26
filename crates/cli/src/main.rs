// SPDX-License-Identifier: AGPL-3.0-only

//! `pkgpod`: the server (`pkgpod serve`) and the command line client of
//! its API.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod client;
mod cmd;
mod term;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pkgpod", version = pkgpod_base::version(), about = "A team package server and its client")]
struct Cli {
    /// Server API URL (default: the saved login, else http://127.0.0.1:8090).
    #[arg(long, global = true, env = "PKGPOD_URL", help_heading = "Server")]
    url: Option<String>,
    /// API token (default: the saved login's, for its server only).
    #[arg(
        long,
        global = true,
        env = "PKGPOD_TOKEN",
        hide_env_values = true,
        help_heading = "Server"
    )]
    token: Option<String>,
    /// Print the API's JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
pub(crate) struct ServerArgs {
    /// Configuration file.
    #[arg(long, short, global = true, default_value = pkgpod_server::config::DEFAULT_PATH, env = "PKGPOD_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Run the server.
    Serve(ServerArgs),
    /// Server commands that work on the database directly (no server
    /// needed): set a password, back up, export and import.
    Admin {
        #[command(flatten)]
        server: ServerArgs,
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Log in and save a token for later commands.
    ///
    /// In a CI job, `--oidc` logs in with the CI's OIDC token instead of
    /// a password (GitHub Actions: `permissions: id-token: write`), as a
    /// trust rule on the server allows; the token saved lasts minutes.
    Login {
        /// The user to log in as (the password is asked for).
        #[arg(long, short, required_unless_present = "oidc")]
        user: Option<String>,
        /// Read the password from stdin.
        #[arg(long)]
        password_stdin: bool,
        /// Exchange the CI's OIDC token (GitHub Actions', or the one in
        /// $PKGPOD_OIDC_TOKEN) for a short-lived token.
        #[arg(long, conflicts_with_all = ["user", "password_stdin"])]
        oidc: bool,
        /// The audience to ask the CI's token for (as the trust rule says).
        #[arg(long, default_value = "pkgpod", requires = "oidc")]
        audience: String,
        /// The trust rule to use when several match.
        #[arg(long, requires = "oidc")]
        rule: Option<String>,
    },
    /// Server version and who you are; with a repository, its state.
    Status {
        repo: Option<String>,
        /// A package's latest version and downloads too.
        #[arg(long)]
        package: Option<String>,
    },
    /// Repositories.
    #[command(subcommand)]
    Repo(cmd::RepoCmd),
    /// Upload files into a repository.
    Upload {
        repo: String,
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Upload parameters (deb: dists=a,b component=main replace=true;
        /// path=README.md puts a file beside dists/ instead; files:
        /// path=dir/name).
        #[arg(long = "set", short = 's', value_name = "KEY=VALUE")]
        params: Vec<String>,
    },
    /// Run an operation of the repository's format (deb: dist-set,
    /// dist-remove, remove, copy, move, file-remove; `pkgpod repo
    /// formats` lists them all); shows the plan first.
    Op {
        repo: String,
        op: String,
        /// Parameters (lists comma-separated).
        params: Vec<String>,
        #[command(flatten)]
        yes: cmd::Yes,
    },
    /// List a directory of a repository.
    Ls {
        repo: String,
        #[arg(default_value = "")]
        path: String,
        /// A version number or label.
        #[arg(long, short)]
        version: Option<String>,
        /// The tree clients are served (deb: dists/ and pool/).
        #[arg(long)]
        served: bool,
    },
    /// Versions of a repository.
    Versions { repo: String },
    /// What changed between two versions (to: the current one).
    Diff {
        repo: String,
        from: String,
        to: Option<String>,
    },
    /// Name a version (default: the current one).
    Label {
        repo: String,
        name: String,
        /// The version to name: a number or label (default: the current one).
        #[arg(long, short)]
        version: Option<String>,
        /// A note kept with the label.
        #[arg(long, default_value = "")]
        note: String,
        /// Move an existing label.
        #[arg(long)]
        force: bool,
    },
    /// Remove a label.
    Unlabel { repo: String, name: String },
    /// Make a new version with the state of an old one.
    Rollback {
        repo: String,
        version: String,
        #[command(flatten)]
        yes: cmd::Yes,
    },
    /// Copy paths of one repository into another of the same format; from
    /// a deb mirror, packages (name or name=version) of one of its dists.
    Promote {
        from: String,
        to: String,
        /// Paths; with --dist (from a mirror), packages.
        #[arg(required = true)]
        paths: Vec<String>,
        /// The source's version: a number or label (default: the current
        /// one); not with --dist.
        #[arg(long, short)]
        version: Option<String>,
        /// From a mirror: its dist.
        #[arg(long)]
        dist: Option<String>,
        /// From a mirror: only these components.
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// From a mirror: only these architectures.
        #[arg(long, value_delimiter = ',')]
        architectures: Vec<String>,
        /// The target's dist (default: the mirror's).
        #[arg(long)]
        into: Option<String>,
        /// The target's component (default: the package's).
        #[arg(long)]
        component: Option<String>,
        /// Replace package files of the same name with other content.
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        yes: cmd::Yes,
    },
    /// Remove paths from a repository.
    Rm {
        repo: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[command(flatten)]
        yes: cmd::Yes,
    },
    /// Search packages by name.
    Search {
        query: String,
        /// Only in this repository.
        #[arg(long, short)]
        repo: Option<String>,
        /// At most this many (the server's default: 200).
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Apply a plan shown earlier.
    Apply { plan: String },
    /// Users.
    #[command(subcommand)]
    User(cmd::UserCmd),
    /// Teams.
    #[command(subcommand)]
    Team(cmd::TeamCmd),
    /// Rights on repositories.
    #[command(subcommand)]
    Grant(cmd::GrantCmd),
    /// API tokens.
    #[command(subcommand)]
    Token(cmd::TokenCmd),
    /// Trusted publishing: which CI jobs (by their OIDC token's claims)
    /// get a short-lived token of which user (server admins).
    #[command(subcommand)]
    Trust(cmd::TrustCmd),
    /// Mirror schedules: each mirror's, when it runs next, and the syncs
    /// of the next day.
    #[command(subcommand)]
    Schedule(cmd::ScheduleCmd),
    /// Host pages: what a host's root shows (its repositories, with a
    /// title and Markdown above and below them; server admins).
    #[command(subcommand)]
    Host(cmd::HostCmd),
    /// Outgoing webhooks: new versions, labels and jobs sent to other
    /// services (server admins).
    #[command(subcommand)]
    Webhook(cmd::WebhookCmd),
    /// Serving listeners.
    #[command(subcommand)]
    Listener(cmd::ListenerCmd),
    /// Signing keys.
    #[command(subcommand)]
    Key(cmd::KeyCmd),
    /// Storages.
    #[command(subcommand)]
    Storage(cmd::StorageCmd),
    /// Start a sync of a mirror.
    Sync {
        repo: String,
        /// Wait for it and print its log.
        #[arg(long, short)]
        wait: bool,
    },
    /// Import into a hosted repository, as one version: an apt archive
    /// (deb), a tree (files) or images (oci).
    Import {
        repo: String,
        /// deb: the archive root (http(s)://host/debian/); files: the
        /// directory (rsync://, http(s):// listings). Both: a directory
        /// on the server (/srv/apt/), read where it is; server admins only.
        source: String,
        /// deb: the dists to import.
        #[arg(long, value_delimiter = ',')]
        dists: Vec<String>,
        /// files: where the tree goes; oci: put before the image names;
        /// default: the top.
        #[arg(long)]
        path: Option<String>,
        /// files: paths left out (rsync-like patterns).
        #[arg(long = "exclude", value_delimiter = ',')]
        excludes: Vec<String>,
        /// oci: images of the registry SOURCE (https://host, docker.io):
        /// name[:tag] or name@sha256:...
        #[arg(long = "image", value_delimiter = ',')]
        images: Vec<String>,
        /// oci: only these platforms (linux/amd64, linux/arm64/v8).
        #[arg(long = "platform", value_delimiter = ',')]
        platforms: Vec<String>,
        /// oci: a user for the source registry; the password is read
        /// from standard input.
        #[arg(long)]
        username: Option<String>,
        /// deb: only these components.
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// deb: only these architectures.
        #[arg(long, value_delimiter = ',')]
        architectures: Vec<String>,
        /// The upstream's public key (armored file) to check signatures.
        #[arg(long)]
        keyring: Option<PathBuf>,
        /// Replace package files of the same name with other content.
        #[arg(long)]
        replace: bool,
        /// Leave out the source packages the upstream lists.
        #[arg(long)]
        no_sources: bool,
        /// Wait for it and print its log.
        #[arg(long, short)]
        wait: bool,
    },
    /// Recent jobs (of one repository, or all).
    Jobs {
        repo: Option<String>,
        /// Only those in this state: queued, running, ok, failed,
        /// cancelled, or active (queued or running); server admins.
        #[arg(long)]
        state: Option<String>,
        /// Only syncs or imports (server admins).
        #[arg(long)]
        kind: Option<String>,
        /// At most this many, the newest first.
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Only jobs older than this one (the next page).
        #[arg(long)]
        before: Option<i64>,
    },
    /// The server's recent log, since it started (server admins).
    Logs {
        /// error, warn, info, debug, trace: that or more severe.
        #[arg(long, default_value = "info")]
        level: String,
        /// Only events with this text.
        #[arg(long)]
        grep: Option<String>,
        /// How many of the newest.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// Keep printing new events as they come.
        #[arg(short, long)]
        follow: bool,
    },
    /// A job and its log.
    Job {
        id: i64,
        /// Stop it.
        #[arg(long)]
        cancel: bool,
    },
    /// What a repository's cleanup rules remove now (a plan; -y
    /// applies); without one, the daily cleanup of every repository, now
    /// (server admin).
    Cleanup {
        repo: Option<String>,
        #[command(flatten)]
        yes: cmd::Yes,
    },
    /// A proxy's cache: list it (by path prefix), or pin, unpin, evict or
    /// prefetch paths.
    Cache {
        repo: String,
        /// list, pin, unpin, evict or prefetch.
        #[arg(default_value = "list")]
        action: String,
        /// Paths (list: a prefix).
        paths: Vec<String>,
    },
    /// Downloads of a repository, by day and by package.
    Downloads {
        repo: String,
        /// The last this many days, today included.
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// One package only.
        #[arg(long, default_value = "")]
        name: String,
    },
    /// A private repository's badge token (repository admin): shows its
    /// badges at /.well-known/pkgpod/badges/<repo>/<kind>.svg?token=.
    Badge {
        repo: String,
        /// Make a new one (the old one stops working).
        #[arg(long, conflicts_with = "remove")]
        new: bool,
        /// Remove it: the badges are not shown.
        #[arg(long)]
        remove: bool,
    },
    /// Show the server's settings, or change them (server admin).
    Settings {
        /// Days download counts are kept (0: none counted).
        #[arg(long)]
        downloads_days: Option<i64>,
        /// Mirror syncs that run at once on one storage (0: no limit).
        #[arg(long)]
        syncs_per_storage: Option<i64>,
        /// Days ended jobs and their logs are kept (0: all).
        #[arg(long)]
        jobs_days: Option<i64>,
        /// The console's public address (https://pkg.example.org), named in
        /// the CLI's install script and CI examples; "" clears it.
        #[arg(long)]
        console_url: Option<String>,
        /// Where the pkgpod GitHub action is (owner/repository@ref); ""
        /// clears it.
        #[arg(long)]
        ci_action: Option<String>,
    },
    /// Collect garbage.
    Gc {
        /// Only count what would be removed.
        #[arg(long)]
        dry_run: bool,
        /// Keep what is younger than this.
        #[arg(long, default_value_t = 24)]
        grace_hours: u64,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    /// Set a user's password (creating the user as an admin if missing).
    Password {
        user: String,
        /// Read the password from stdin.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Back up the data into an empty directory: the database, the
    /// signing keys, the content stores (hard links on the same
    /// filesystem), with --mirrors the mirrors' trees. Safe while the
    /// server runs.
    Backup {
        to: PathBuf,
        /// The mirrors' received trees too.
        #[arg(long)]
        mirrors: bool,
    },
    /// Restore a backup into the configured data directory, which must
    /// be empty (the server not running on it).
    Restore { from: PathBuf },
    /// Write a portable dump of the data into an empty directory: what
    /// a pkgpod with another database schema can import (a backup only
    /// restores into the same schema). Users, rights, pages, settings,
    /// keys, each repository's settings and current tree, proxies'
    /// caches; not sessions, jobs, older versions. Safe while the server
    /// runs; exits 1 if blobs it names are missing (still written).
    Export {
        /// A new or empty directory (mode 0700).
        to: PathBuf,
        /// The blobs named: none (found in the stores at import), link
        /// (hard links, same filesystem only) or copy.
        #[arg(long, default_value = "none", value_parser = ["none", "link", "copy"])]
        blobs: String,
        /// API tokens (their hashes) too.
        #[arg(long)]
        tokens: bool,
        /// Download counts too.
        #[arg(long)]
        downloads: bool,
        /// The audit log too (kept with the dump, not imported).
        #[arg(long)]
        audit: bool,
    },
    /// Take a dump (from admin export) into a new data directory: the
    /// configured one, with no database yet, the server not running on
    /// it. Repositories are taken in whole or not at all; trust rules
    /// and API tokens are made again (it prints what to make). Exits 1 if
    /// a repository could not be taken in or the check after it finds
    /// problems.
    Import {
        /// The dump's directory.
        from: PathBuf,
        /// A store directory (a storage's store/) to take blobs from that
        /// the dump does not hold (hard links where they can be); may be
        /// given more than once.
        #[arg(long = "blobs-from", value_name = "DIR")]
        blobs_from: Vec<PathBuf>,
        /// A storage somewhere else than the dump says (the storage main
        /// is always the new data directory's).
        #[arg(long = "storage", value_name = "NAME=PATH")]
        storages: Vec<String>,
        /// Keep the new data directory's listener rather than the dump's.
        #[arg(long)]
        keep_listeners: bool,
    },
    /// Check that every version can be read whole and every blob it
    /// names is in the store (with --deep, with its sha256); exits 1 on
    /// problems. Safe while the server runs.
    Check {
        /// Each blob's sha256 too (reads them all).
        #[arg(long)]
        deep: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !matches!(cli.cmd, Cmd::Serve(_)) {
        quiet_when_output_is_cut();
    }
    if matches!(cli.cmd, Cmd::Serve(_) | Cmd::Admin { .. }) {
        // The database, keys and stores are the server's and its group's.
        // SAFETY: plain libc call before any thread starts.
        unsafe {
            libc::umask(0o027);
        }
    }
    let r = match cli.cmd {
        Cmd::Serve(a) => serve(&a),
        Cmd::Admin { server, cmd } => admin(&server, cmd),
        cmd => cmd::run(cli.url, cli.token, cli.json, cmd),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pkgpod: {}", e.message);
            ExitCode::from(e.code)
        }
    }
}

/// Output piped into `head`: end quietly when the reader goes away.
/// (Not by SIGPIPE's default action: that ends the process on any
/// closed pipe or socket without a word, a server on a client that gave
/// up on a download, the CLI on an upload the server refused.)
fn quiet_when_output_is_cut() {
    let next = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let p = info.payload();
        let msg = p
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| p.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if msg.starts_with("failed printing to stdout") && msg.contains("Broken pipe") {
            std::process::exit(0);
        }
        next(info);
    }));
}

fn load(a: &ServerArgs) -> Result<pkgpod_server::config::Config, client::CliError> {
    pkgpod_server::config::Config::load(&a.config).map_err(client::CliError::usage)
}

fn serve(a: &ServerArgs) -> Result<(), client::CliError> {
    let cfg = load(a)?;
    // To stdout (the journal), and the recent events kept for the
    // console's Logs page. Colours only on a terminal (not in a file or
    // the journal), and no time of our own where the journal stamps
    // each line.
    use std::io::IsTerminal;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let ansi = std::io::stdout().is_terminal();
    let journal = stdout_is_journal();
    let out = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(ansi);
    let out = if journal {
        out.without_time().boxed()
    } else {
        out.boxed()
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_env("PKGPOD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.log)),
        )
        .with(out)
        .with(pkgpod_server::ops::logs::layer())
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| client::CliError::other(e.to_string()))?;
    let r = rt.block_on(async {
        let app =
            pkgpod_server::App::open(cfg).map_err(|e| client::CliError::other(e.to_string()))?;
        pkgpod_server::run(app)
            .await
            .map_err(client::CliError::other)
    });
    // Blocking work still running (a GC pass, a long upload's ingest)
    // does not hold the exit up once the requests have been answered.
    rt.shutdown_timeout(std::time::Duration::from_secs(5));
    r
}

/// Whether stdout is the journal: the stream systemd names in
/// JOURNAL_STREAM ("<dev>:<ino>"). The variable alone is not enough:
/// whatever a service starts inherits it, a shell in a terminal too.
fn stdout_is_journal() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let Some(j) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let m = std::io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata());
    m.is_ok_and(|m| j.to_str() == Some(&format!("{}:{}", m.dev(), m.ino())))
}

fn admin(a: &ServerArgs, cmd: AdminCmd) -> Result<(), client::CliError> {
    let cfg = load(a)?;
    let err = |e: pkgpod_server::Error| client::CliError::other(e.to_string());
    let show = |rep: &pkgpod_server::jobs::backup::Report| {
        println!(
            "{} file(s) ({} linked, {} bytes copied)",
            rep.files, rep.linked, rep.bytes
        );
        for w in &rep.warnings {
            println!("warning: {w}");
        }
    };
    if let AdminCmd::Restore { from } = &cmd {
        // Before the server's data is opened, which would make it.
        let rep =
            pkgpod_server::jobs::backup::restore(from, &cfg.data_dir, &mut |m| println!("{m}"))
                .map_err(err)?;
        show(&rep);
        println!("restored into {}", cfg.data_dir.display());
        return Ok(());
    }
    if let AdminCmd::Import {
        from,
        blobs_from,
        storages,
        keep_listeners,
    } = &cmd
    {
        return import(cfg, from, blobs_from, storages, *keep_listeners);
    }
    if matches!(cmd, AdminCmd::Export { .. }) && !cfg.db_path().is_file() {
        // Opening would make a new, empty data directory to export.
        return Err(client::CliError::other(format!(
            "no pkgpod data in {}",
            cfg.data_dir.display()
        )));
    }
    let app = pkgpod_server::App::open(cfg).map_err(err)?;
    match cmd {
        AdminCmd::Password {
            user,
            password_stdin,
        } => {
            let pw = if password_stdin {
                term::read_stdin_line()
            } else {
                term::read_password(&format!("New password for {user}: "))
            }
            .map_err(|e| client::CliError::other(format!("reading the password: {e}")))?;
            if app.auth.user(&user).is_ok() {
                app.auth.set_password(&user, &pw).map_err(err)?;
                println!("password of {user} changed");
            } else {
                app.auth.create_user(&user, &pw, true).map_err(err)?;
                println!("admin {user} created");
            }
        }
        AdminCmd::Backup { to, mirrors } => {
            let rep =
                pkgpod_server::jobs::backup::backup(&app, &to, mirrors, &mut |m| println!("{m}"))
                    .map_err(err)?;
            show(&rep);
            println!("backed up into {}", to.display());
        }
        AdminCmd::Restore { .. } | AdminCmd::Import { .. } => unreachable!("handled above"),
        AdminCmd::Export {
            to,
            blobs,
            tokens,
            downloads,
            audit,
        } => {
            use pkgpod_server::jobs::dump;
            let opt = dump::ExportOptions {
                blobs: dump::Blobs::parse(&blobs).map_err(err)?,
                tokens,
                downloads,
                audit,
            };
            let rep = dump::export(&app, &to, opt, &mut |m| println!("{m}")).map_err(err)?;
            println!(
                "{} blob(s) named, {} bytes; {} linked, {} copied into the dump",
                rep.blobs, rep.bytes, rep.linked, rep.copied
            );
            for w in &rep.warnings {
                println!("warning: {w}");
            }
            for m in &rep.missing {
                println!("missing: {} {} {}: {}", m.repo, m.path, m.sha256, m.problem);
            }
            println!("exported into {}", to.display());
            if !rep.missing.is_empty() {
                return Err(client::CliError::other(format!(
                    "{} blob(s) the dump names are missing (listed in DUMP.json)",
                    rep.missing.len()
                )));
            }
        }
        AdminCmd::Check { deep } => {
            let rep = app
                .engine
                .check(deep)
                .map_err(|e| client::CliError::other(e.to_string()))?;
            println!(
                "{} repositories, {} versions, {} tree objects, {} blobs{}",
                rep.repos,
                rep.versions,
                rep.objects,
                rep.blobs,
                if deep { " (hashed)" } else { "" }
            );
            for p in &rep.problems {
                println!("problem: {p}");
            }
            if rep.more() > 0 {
                println!("... and {} more", rep.more());
            }
            if !rep.ok() {
                return Err(client::CliError::other(format!(
                    "{} problem(s) found",
                    rep.total
                )));
            }
            println!("ok");
        }
    }
    Ok(())
}

/// `admin import`: into a new data directory, held locked.
fn import(
    cfg: pkgpod_server::config::Config,
    from: &std::path::Path,
    blobs_from: &[PathBuf],
    storages: &[String],
    keep_listeners: bool,
) -> Result<(), client::CliError> {
    use pkgpod_server::jobs::dump;
    if !from.join("DUMP.json").is_file() {
        return Err(client::CliError::usage(format!(
            "{}: not a whole dump (no DUMP.json)",
            from.display()
        )));
    }
    let mut opt = dump::ImportOptions {
        blobs_from: blobs_from.to_vec(),
        storages: Vec::new(),
        keep_listeners,
    };
    for s in storages {
        let Some((name, path)) = s.split_once('=') else {
            return Err(client::CliError::usage(format!("--storage {s}: NAME=PATH")));
        };
        opt.storages.push((name.to_string(), PathBuf::from(path)));
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(&cfg.data_dir)
            .map_err(|e| client::CliError::other(format!("{}: {e}", cfg.data_dir.display())))?;
    }
    // Held to the end: no server starts on the data meanwhile.
    let _lock = pkgpod_server::lock_data_dir(&cfg.data_dir).map_err(client::CliError::usage)?;
    if cfg.db_path().exists() {
        return Err(client::CliError::usage(format!(
            "{} has a database already: an import goes into a new data directory",
            cfg.data_dir.display()
        )));
    }
    // From here on a failure leaves the data half made: a rerun refuses
    // it, so say so.
    let dir = cfg.data_dir.clone();
    let half = move |e: pkgpod_server::Error| {
        client::CliError::other(format!(
            "{e}\n{} is half made now: remove it before importing again",
            dir.display()
        ))
    };
    let app = pkgpod_server::App::open(cfg).map_err(&half)?;
    let rep = dump::import(&app, from, &opt, &mut |m| println!("{m}")).map_err(&half)?;
    for r in &rep.repos {
        let mut line = format!("{} ({})", r.name, r.kind);
        if r.kind == "hosted" {
            line.push_str(&format!(
                ": {} entries; blobs {} there, {} linked, {} copied",
                r.entries, r.present, r.linked, r.copied
            ));
        }
        if r.kind == "proxy" {
            line.push_str(&format!(
                ": {} cache rows ({} left out)",
                r.cache_rows, r.cache_skipped
            ));
        }
        match r.same_root {
            Some(true) => line.push_str("; the tree exported"),
            Some(false) => line.push_str("; a tree other than the one exported"),
            None => {}
        }
        println!("{line}");
    }
    for (n, why) in &rep.failed {
        println!("not taken in: {n}: {why}");
    }
    for w in &rep.warnings {
        println!("warning: {w}");
    }
    for p in &rep.problems {
        println!("problem: {p}");
    }
    for m in &rep.remake {
        println!("to make again: {m}");
    }
    if !rep.failed.is_empty() || !rep.problems.is_empty() {
        return Err(client::CliError::other(format!(
            "{} repositories not taken in, {} problem(s) found",
            rep.failed.len(),
            rep.problems.len()
        )));
    }
    println!("imported {} repositories", rep.repos.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Every command and option says in its help what it is.
    #[test]
    fn help_describes_every_command_and_option() {
        Cli::command().debug_assert();
        fn walk(c: &clap::Command, at: &str, missing: &mut Vec<String>) {
            for a in c.get_arguments() {
                // clap's own --help and --version, by what they do: an
                // option of ours may be called version too.
                let own = matches!(
                    a.get_action(),
                    clap::ArgAction::Help
                        | clap::ArgAction::HelpShort
                        | clap::ArgAction::HelpLong
                        | clap::ArgAction::Version
                );
                if !a.is_positional() && a.get_help().is_none() && !own {
                    missing.push(format!("{at} --{}", a.get_id()));
                }
            }
            for sub in c.get_subcommands() {
                let at = format!("{at} {}", sub.get_name());
                if sub.get_about().is_none() {
                    missing.push(at.clone());
                }
                walk(sub, &at, missing);
            }
        }
        let mut missing = Vec::new();
        walk(&Cli::command(), "pkgpod", &mut missing);
        assert!(missing.is_empty(), "no help: {missing:#?}");
    }

    /// `admin -c` before or after its subcommand.
    #[test]
    fn admin_config_anywhere() {
        for args in [
            ["pkgpod", "admin", "-c", "/x.toml", "check"],
            ["pkgpod", "admin", "check", "-c", "/x.toml"],
        ] {
            match Cli::try_parse_from(args).unwrap().cmd {
                Cmd::Admin { server, .. } => {
                    assert_eq!(server.config, PathBuf::from("/x.toml"), "{args:?}")
                }
                _ => panic!("{args:?}"),
            }
        }
    }
}
