//! `hkc-mcp`: an MCP server that lets Claude read and control a Hyundai or
//! Kia (India) car through the `hkconnect` library.
//!
//! ```text
//! hkc-mcp stdio                 # local: Claude Desktop / Claude Code
//! hkc-mcp serve --listen 127.0.0.1:8787 --public-url https://car.example.com
//! hkc-mcp set-password          # set/rotate the sign-in password (hashed)
//! hkc-mcp revoke-all            # drop every issued token
//! ```
//!
//! See `docs/mcp.md` in the repository for the setup guide.

mod audit;
mod format;
mod http;
mod oauth;
mod policy;
mod store;
mod tools;

#[cfg(test)]
mod tests;

use std::io::BufRead as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use hkconnect::{Brand, Client, Config, Credentials};
use rmcp::ServiceExt as _;
use tokio_util::sync::CancellationToken;

use crate::audit::AuditLog;
use crate::oauth::state::StateFile;
use crate::oauth::{OAuth, OAuthConfig};
use crate::policy::Policy;
use crate::store::{EnvFile, Store};
use crate::tools::{CarServer, Settings};

/// Longest wait for a command's answer: Claude gives up on a tool call
/// after 240 s, and sending the command can itself take up to a minute.
const MAX_COMMAND_WAIT_SECS: u64 = 170;
/// Shortest sign-in password.
const MIN_PASSWORD_LEN: usize = 12;

// Settings come from, in order: a command-line flag, the real environment,
// then the `.env` file in the config directory. The `.env` file is parsed
// literally by `store::parse_env` (no `$VAR` substitution) and is never
// copied into the process environment, so clap's own `env` support is not
// used; each flag names its variable in its help instead.

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Where the session, OAuth state and audit log are kept
    /// [env: `HKC_MCP_DATA_DIR`].
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Car {
    /// Car brand, hyundai or kia [env: `HKC_BRAND`; default hyundai].
    #[arg(long)]
    brand: Option<Brand>,
    /// Default car (name, VIN or id) when a tool call names none
    /// [env: `HKC_VEHICLE`].
    #[arg(long)]
    vehicle: Option<String>,
    /// Extra tools to offer, comma-separated (e.g. `unlock,start_climate`),
    /// or `all` [env: `HKC_MCP_ENABLE`].
    #[arg(long)]
    enable: Option<String>,
    /// Default tools to hide, comma-separated [env: `HKC_MCP_DISABLE`].
    #[arg(long)]
    disable: Option<String>,
    /// Minutes between car-waking status refreshes of one car, 0 for no
    /// limit [env: `HKC_MCP_REFRESH_MINUTES`; default 10].
    #[arg(long)]
    refresh_minutes: Option<u64>,
    /// Seconds a command waits for the car's answer
    /// [env: `HKC_MCP_COMMAND_WAIT_SECONDS`; default 90].
    #[arg(long)]
    command_wait_seconds: Option<u64>,
    /// Log API calls (never secrets) to stderr.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Serve MCP over stdin/stdout (Claude Desktop, Claude Code).
    Stdio(Car),
    /// Serve MCP over HTTP with OAuth (claude.ai custom connector).
    Serve {
        #[command(flatten)]
        car: Car,
        /// Address to listen on. Keep it on loopback behind a tunnel or
        /// reverse proxy that terminates TLS
        /// [env: `HKC_MCP_LISTEN`; default 127.0.0.1:8787].
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// The public https origin clients use, e.g. `https://car.example.com`.
        /// The MCP endpoint is this URL followed by `/mcp`
        /// [env: `HKC_MCP_PUBLIC_URL`].
        #[arg(long)]
        public_url: Option<String>,
        /// Trust `X-Forwarded-For` from the proxy in front for the client
        /// address (sign-in rate limiting). Only set it if a proxy always
        /// sets that header [env: `HKC_MCP_TRUST_PROXY`=true].
        #[arg(long)]
        trust_proxy: bool,
        /// Accept dynamic client registration at /register
        /// [env: `HKC_MCP_ALLOW_DCR`=true].
        #[arg(long)]
        allow_dcr: bool,
        /// Only these OAuth client ids may connect, comma-separated
        /// [env: `HKC_MCP_ALLOWED_CLIENTS`].
        #[arg(long)]
        allowed_clients: Option<String>,
    },
    /// Set or change the sign-in password for `serve`.
    SetPassword {
        /// Read the password from the first line of stdin instead of
        /// prompting (for scripts and containers).
        #[arg(long)]
        password_stdin: bool,
        /// Keep already-issued tokens (by default changing the password
        /// signs every client out).
        #[arg(long)]
        keep_tokens: bool,
    },
    /// Revoke every access and refresh token; clients must sign in again.
    RevokeAll {
        /// Also forget dynamically registered clients.
        #[arg(long)]
        clients: bool,
    },
}

/// A flag's value, else the named variable parsed as `T`.
fn setting<T>(flag: Option<T>, env: &EnvFile, name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    if flag.is_some() {
        return Ok(flag);
    }
    env.var(name)
        .map(|v| {
            v.trim()
                .parse::<T>()
                .map_err(|e| anyhow::anyhow!("invalid {name}: {e}"))
        })
        .transpose()
}

/// A boolean switch: the flag, else the named variable.
fn switch(flag: bool, env: &EnvFile, name: &str) -> Result<bool> {
    if flag {
        return Ok(true);
    }
    match env
        .var(name)
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("0" | "false" | "no" | "off") => Ok(false),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some(_) => bail!("invalid {name}: use true or false"),
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // `.env` lives in the config directory, which no flag moves.
    let env = Store::open(None).map(|s| s.load_env()).unwrap_or_default();
    match run(cli, &env).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: bool) {
    let filter = if verbose {
        "hkconnect=debug,hkc_mcp=debug,info"
    } else {
        "hkc_mcp=info,warn"
    };
    // Always stderr: over stdio, stdout carries the protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

async fn run(cli: Cli, env: &EnvFile) -> Result<()> {
    let data_dir = setting(cli.data_dir, env, "HKC_MCP_DATA_DIR")?;
    let store = Store::open(data_dir)?;
    match cli.command {
        Command::Stdio(car) => {
            init_logging(car.verbose);
            let server = car_server(&car, &store, env)?;
            tracing::info!("serving MCP over stdio");
            let running = server
                .serve(rmcp::transport::stdio())
                .await
                .context("MCP handshake over stdio failed")?;
            running.waiting().await?;
            Ok(())
        }
        Command::Serve {
            car,
            listen,
            public_url,
            trust_proxy,
            allow_dcr,
            allowed_clients,
        } => {
            init_logging(car.verbose);
            let public_url = setting(public_url, env, "HKC_MCP_PUBLIC_URL")?
                .context("set --public-url or HKC_MCP_PUBLIC_URL (e.g. https://car.example.com)")?;
            let listen = setting(listen, env, "HKC_MCP_LISTEN")?
                .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8787)));
            let mut config = OAuthConfig::new(&public_url)?;
            config.trust_proxy = switch(trust_proxy, env, "HKC_MCP_TRUST_PROXY")?;
            config.allow_dcr = switch(allow_dcr, env, "HKC_MCP_ALLOW_DCR")?;
            let allowed_clients = setting(allowed_clients, env, "HKC_MCP_ALLOWED_CLIENTS")?;
            config.allowed_clients = allowed_clients.map(|list| {
                list.split(',')
                    .map(|c| c.trim().to_owned())
                    .filter(|c| !c.is_empty())
                    .collect()
            });
            let oauth = Arc::new(OAuth::new(config, StateFile::new(store.oauth_file())));
            if !oauth.has_password().await? {
                bail!("no sign-in password is set; run `hkc-mcp set-password` first");
            }
            let server = car_server(&car, &store, env)?;
            serve(oauth, server, listen).await
        }
        Command::SetPassword {
            password_stdin,
            keep_tokens,
        } => {
            let password = read_new_password(password_stdin)?;
            let state = StateFile::new(store.oauth_file());
            let revoked = oauth::set_password(&state, &password, !keep_tokens).await?;
            println!("Password saved to {}.", store.oauth_file().display());
            if revoked > 0 {
                println!("Revoked {revoked} issued token(s); clients must sign in again.");
            }
            Ok(())
        }
        Command::RevokeAll { clients } => {
            let state = StateFile::new(store.oauth_file());
            let (tokens, registered) = oauth::revoke_all(&state, clients).await?;
            println!("Revoked {tokens} token(s).");
            if clients {
                println!("Forgot {registered} registered client(s).");
            }
            Ok(())
        }
    }
}

fn credentials_from_env(env: &EnvFile) -> Result<Credentials> {
    let var = |name: &str| env.var(name);
    let username =
        var("HKC_USERNAME").context("set HKC_USERNAME (your Bluelink / Kia Connect email)")?;
    let password = var("HKC_PASSWORD").context("set HKC_PASSWORD")?;
    let credentials = Credentials::new(username, password);
    Ok(match var("HKC_PIN") {
        Some(pin) => credentials.with_pin(pin),
        None => credentials,
    })
}

fn car_server(car: &Car, store: &Store, env: &EnvFile) -> Result<CarServer> {
    let enable = setting(car.enable.clone(), env, "HKC_MCP_ENABLE")?;
    let disable = setting(car.disable.clone(), env, "HKC_MCP_DISABLE")?;
    let policy = Policy::from_lists(enable.as_deref(), disable.as_deref())?;
    let wait = setting(
        car.command_wait_seconds,
        env,
        "HKC_MCP_COMMAND_WAIT_SECONDS",
    )?
    .unwrap_or(90);
    if wait == 0 || wait > MAX_COMMAND_WAIT_SECS {
        bail!("HKC_MCP_COMMAND_WAIT_SECONDS must be 1 to {MAX_COMMAND_WAIT_SECS}");
    }
    let refresh_minutes =
        setting(car.refresh_minutes, env, "HKC_MCP_REFRESH_MINUTES")?.unwrap_or(10);
    let brand = setting(car.brand, env, "HKC_BRAND")?.unwrap_or(Brand::Hyundai);
    let vehicle = setting(car.vehicle.clone(), env, "HKC_VEHICLE")?;
    let credentials = credentials_from_env(env)?;
    let mut client = Client::new(Config::india(brand), credentials.clone())?;
    if let Some(session) = store.load_session(brand, credentials.username()) {
        client = client.with_session(session);
    }
    let settings = Settings {
        brand,
        username: credentials.username().to_owned(),
        default_vehicle: vehicle.filter(|v| !v.trim().is_empty()),
        refresh_interval: Duration::from_secs(refresh_minutes.saturating_mul(60)),
        command_wait: Duration::from_secs(wait),
        poll_every: Duration::from_secs(3),
    };
    tracing::info!(
        tools = %policy.enabled().collect::<Vec<_>>().join(","),
        "tool policy"
    );
    Ok(CarServer::new(
        client,
        settings,
        &policy,
        Some(store.clone()),
        AuditLog::new(store.audit_file()),
    ))
}

async fn serve(oauth: Arc<OAuth>, server: CarServer, listen: SocketAddr) -> Result<()> {
    let shutdown = CancellationToken::new();
    let app = http::app(&oauth, server, shutdown.child_token());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("could not listen on {listen}"))?;
    tracing::info!(
        %listen,
        mcp = %oauth.config().resource(),
        trust_proxy = oauth.config().trust_proxy,
        dcr = oauth.config().allow_dcr,
        "serving MCP over HTTP"
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        tracing::info!("shutting down");
        shutdown.cancel();
    })
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Prompt twice without echo (or read one line from stdin), and check the
/// password is long enough.
fn read_new_password(from_stdin: bool) -> Result<String> {
    let password = if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else {
        let first = rpassword::prompt_password("New hkc-mcp password: ")
            .context("could not read the password (no terminal? use --password-stdin)")?;
        let second = rpassword::prompt_password("Repeat it: ")?;
        if first != second {
            bail!("the passwords don't match; nothing was changed");
        }
        first
    };
    check_password_strength(&password)?;
    Ok(password)
}

fn check_password_strength(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        bail!(
            "use at least {MIN_PASSWORD_LEN} characters: this password guards remote \
             control of your car (a passphrase of several words is easiest)"
        );
    }
    if password.len() > 1024 {
        bail!("the password is too long (1024 bytes at most)");
    }
    Ok(())
}
