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
    /// Server version and who you are.
    Status,
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
    /// dist-remove, remove, copy, move); shows the plan first.
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
    /// Copy paths of one repository into another of the same format.
    Promote {
        from: String,
        to: String,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long, short)]
        version: Option<String>,
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
    /// Recent jobs (of one repository, or all).
    Jobs { repo: Option<String> },
    /// A job and its log.
    Job {
        id: i64,
        /// Stop it.
        #[arg(long)]
        cancel: bool,
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
    /// Write a consistent copy of the database.
    Backup { to: PathBuf },
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
    rt.block_on(async {
        let app =
            pkgpod_server::App::open(cfg).map_err(|e| client::CliError::other(e.to_string()))?;
        pkgpod_server::run(app)
            .await
            .map_err(client::CliError::other)
    })
}

fn admin(a: &ServerArgs, cmd: AdminCmd) -> Result<(), client::CliError> {
    let cfg = load(a)?;
    let err = |e: pkgpod_server::Error| client::CliError::other(e.to_string());
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
        AdminCmd::Backup { to } => {
            app.db
                .backup(&to)
                .map_err(|e| client::CliError::other(e.to_string()))?;
            println!("database copied to {}", to.display());
        }
    }
    Ok(())
}
