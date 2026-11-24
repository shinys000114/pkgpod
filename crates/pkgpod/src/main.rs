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
    #[arg(long, global = true, env = "PKGPOD_URL")]
    url: Option<String>,
    /// API token (default: the saved login).
    #[arg(long, global = true, env = "PKGPOD_TOKEN", hide_env_values = true)]
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
    #[arg(long, short, default_value = pkgpod_server::config::DEFAULT_PATH, env = "PKGPOD_CONFIG")]
    config: PathBuf,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Run the server.
    Serve(ServerArgs),
    /// Server commands that work on the database directly (no server
    /// needed): set a password, back up.
    Admin {
        #[command(flatten)]
        server: ServerArgs,
        #[command(subcommand)]
        cmd: AdminCmd,
    },
    /// Log in and save a token for later commands.
    Login {
        #[arg(long, short)]
        user: String,
        /// Read the password from stdin.
        #[arg(long)]
        password_stdin: bool,
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
        /// Upload parameters (deb: dists=a,b component=main replace=true).
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
        #[arg(long, short)]
        version: Option<String>,
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
        #[arg(long, short)]
        version: Option<String>,
        /// From a mirror: its dist.
        #[arg(long)]
        dist: Option<String>,
        /// From a mirror: only these components / architectures.
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        architectures: Vec<String>,
        /// The target's dist (default: the mirror's) and component
        /// (default: the package's).
        #[arg(long)]
        into: Option<String>,
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
        #[arg(long, short)]
        repo: Option<String>,
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
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
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
    Jobs { repo: Option<String> },
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
    },
    /// Collect garbage.
    Gc {
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
        #[arg(long)]
        password_stdin: bool,
    },
    /// Back up the data into an empty directory: the database, the
    /// signing keys, the content stores (hard links on the same
    /// filesystem), with --mirrors the mirrors' trees. Safe while the
    /// server runs.
    Backup {
        to: PathBuf,
        #[arg(long)]
        mirrors: bool,
    },
    /// Restore a backup into the configured data directory, which must
    /// be empty (the server not running on it).
    Restore { from: PathBuf },
    /// Check that every version can be read whole and every blob it
    /// names is in the store (with --deep, with its sha256); exits 1 on
    /// problems. Safe while the server runs.
    Check {
        #[arg(long)]
        deep: bool,
    },
}

fn main() -> ExitCode {
    // Output piped into `head`: end quietly when the reader goes away.
    // SAFETY: setting a signal disposition before any thread starts.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
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

fn load(a: &ServerArgs) -> Result<pkgpod_server::config::Config, client::CliError> {
    pkgpod_server::config::Config::load(&a.config).map_err(client::CliError::usage)
}

fn serve(a: &ServerArgs) -> Result<(), client::CliError> {
    let cfg = load(a)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PKGPOD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&cfg.log)),
        )
        .with_target(false)
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

fn admin(a: &ServerArgs, cmd: AdminCmd) -> Result<(), client::CliError> {
    let cfg = load(a)?;
    let err = |e: pkgpod_server::Error| client::CliError::other(e.to_string());
    let show = |rep: &pkgpod_server::backup::Report| {
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
        let rep = pkgpod_server::backup::restore(from, &cfg.data_dir, &mut |m| println!("{m}"))
            .map_err(err)?;
        show(&rep);
        println!("restored into {}", cfg.data_dir.display());
        return Ok(());
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
            let rep = pkgpod_server::backup::backup(&app, &to, mirrors, &mut |m| println!("{m}"))
                .map_err(err)?;
            show(&rep);
            println!("backed up into {}", to.display());
        }
        AdminCmd::Restore { .. } => unreachable!("handled above"),
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
            if !rep.ok() {
                return Err(client::CliError::other(format!(
                    "{} problem(s) found",
                    rep.problems.len()
                )));
            }
            println!("ok");
        }
    }
    Ok(())
}
