use std::{
    io::{self, IsTerminal},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use clap::{Parser, Subcommand};
use trama_collab::{
    AuthConfig, AuthService, ForwardedHeader, IpCidr, MAX_PASSWORD_BYTES, ProxyTrust, probe_health,
    read_secret, restore_database, router_with_proxy, validate_bind,
};

#[derive(Parser)]
#[command(about = "Collaboration service for TramaWorkspace (shared threads)")]
struct Args {
    /// SQLite database file.
    #[arg(
        long,
        global = true,
        env = "TRAMA_COLLAB_DB",
        default_value = "trama-auth.sqlite"
    )]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    CreateUser {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Provision a runner credential for an existing user. The token is
    /// printed once; only its digest is stored. The runner may only claim
    /// turns authored by that user.
    CreateRunner {
        #[arg(long)]
        username: String,
        #[arg(long)]
        name: String,
    },
    /// Write a consistent snapshot of the database (safe while serving).
    /// The file must not exist; it is created owner-only.
    Backup {
        #[arg(long)]
        to: PathBuf,
    },
    /// Replace the database with a snapshot made by `backup`. Stop the
    /// service first; the current database is kept beside the new one.
    Restore {
        #[arg(long)]
        from: PathBuf,
    },
    /// Exit 0 when a local server answers `GET /healthz` (container health
    /// check; the image has no curl).
    Healthcheck {
        /// Address to probe; an unspecified address (0.0.0.0) probes loopback.
        #[arg(long, env = "TRAMA_COLLAB_BIND", default_value = "127.0.0.1:8787")]
        addr: SocketAddr,
    },
    Serve {
        #[arg(long, env = "TRAMA_COLLAB_BIND", default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
        /// Run behind a TLS-terminating reverse proxy on a private network.
        /// Allows a non-loopback bind (the listener speaks plain HTTP; never
        /// expose it directly) and honors the forwarding header, but only
        /// from peers in --trusted-proxy. Default: off.
        #[arg(
            long,
            env = "TRAMA_COLLAB_BEHIND_PROXY",
            action = clap::ArgAction::SetTrue,
            value_parser = clap::builder::BoolishValueParser::new()
        )]
        behind_proxy: bool,
        /// Networks (CIDR, comma separated) whose peers may set the client
        /// address via the forwarding header. Default: loopback, RFC 1918
        /// and fc00::/7. Only used with --behind-proxy.
        #[arg(
            long = "trusted-proxy",
            env = "TRAMA_COLLAB_TRUSTED_PROXIES",
            value_delimiter = ','
        )]
        trusted_proxies: Vec<IpCidr>,
        /// Forwarding header the proxy maintains: x-forwarded-for or
        /// forwarded. Only that header is read.
        #[arg(
            long,
            env = "TRAMA_COLLAB_FORWARDED_HEADER",
            default_value = "x-forwarded-for"
        )]
        forwarded_header: ForwardedHeader,
        /// Public server identity in `/.well-known/trama-collab`. Default: a
        /// random id generated once and kept in the database.
        #[arg(long, env = "TRAMA_COLLAB_SERVER_ID")]
        server_id: Option<String>,
    },
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match args.command {
        Command::CreateUser {
            username,
            password_stdin,
        } => {
            let password = if password_stdin {
                // An interactive terminal would echo the password; refuse and
                // point at the hidden-prompt flow instead.
                if io::stdin().is_terminal() {
                    return Err(
                        "refusing to read a password from an interactive terminal because it \
                         would be echoed; rerun without --password-stdin for a hidden prompt"
                            .into(),
                    );
                }
                read_secret(io::stdin().lock(), MAX_PASSWORD_BYTES)?
            } else {
                let password = rpassword::prompt_password("New password (minimum 10 bytes): ")?;
                let confirmation = rpassword::prompt_password("Confirm password: ")?;
                if password != confirmation {
                    return Err("passwords do not match".into());
                }
                password
            };
            let auth = AuthService::open_admin(args.db, AuthConfig::default())?;
            auth.create_user(&username, &password).await?;
            println!("Created user {username}");
        }
        Command::CreateRunner { username, name } => {
            let auth = AuthService::open_admin(args.db, AuthConfig::default())?;
            let token = auth.create_runner(&username, &name).await?;
            eprintln!(
                "Created runner {name} for {username}. Store this token now; it is not shown again:"
            );
            println!("{token}");
        }
        Command::Backup { to } => {
            let auth = AuthService::open_admin(args.db, AuthConfig::default())?;
            auth.backup_to(&to)?;
            println!("Wrote backup {}", to.display());
        }
        Command::Restore { from } => {
            restore_database(&args.db, &from)?;
            println!("Restored {} from {}", args.db.display(), from.display());
        }
        Command::Healthcheck { addr } => {
            let ip = if addr.ip().is_unspecified() {
                IpAddr::from([127, 0, 0, 1])
            } else {
                addr.ip()
            };
            if let Err(error) =
                probe_health(SocketAddr::new(ip, addr.port()), Duration::from_secs(3))
            {
                eprintln!("unhealthy: {error}");
                std::process::exit(1);
            }
        }
        Command::Serve {
            bind,
            behind_proxy,
            trusted_proxies,
            forwarded_header,
            server_id,
        } => {
            let proxy = if behind_proxy {
                let networks = if trusted_proxies.is_empty() {
                    ProxyTrust::default_networks()
                } else {
                    trusted_proxies
                };
                if !bind.ip().is_loopback() {
                    eprintln!(
                        "WARNING: --behind-proxy with non-loopback bind {bind}. This listener \
                         speaks plain HTTP: bearer tokens and passwords are readable by anyone \
                         who can reach it. Keep it on a private network and publish only the \
                         TLS proxy."
                    );
                }
                Some(ProxyTrust::new(networks, forwarded_header))
            } else {
                validate_bind(bind)?;
                None
            };
            let auth = AuthService::open(
                args.db,
                AuthConfig {
                    server_id,
                    ..AuthConfig::default()
                },
            )?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            match &proxy {
                Some(trust) => println!(
                    "Collaboration service listening on http://{bind} behind a proxy \
                     (trusting {} network(s) for the {} header)",
                    trust.trusted().len(),
                    trust.header().as_str()
                ),
                None => println!("Collaboration service listening on http://{bind}"),
            }
            // Stop accepting on SIGTERM/SIGINT, then give open sockets a few
            // seconds before exiting so `docker stop` is not a SIGKILL.
            let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
            tokio::spawn(async move {
                shutdown_signal().await;
                let _ = stop_tx.send(true);
            });
            let mut graceful_rx = stop_rx.clone();
            let server = axum::serve(
                listener,
                router_with_proxy(auth, proxy).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = graceful_rx.wait_for(|stopped| *stopped).await;
            });
            tokio::select! {
                result = server => result?,
                () = async {
                    let _ = stop_rx.wait_for(|stopped| *stopped).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                } => eprintln!("shutdown grace period elapsed; closing remaining connections"),
            }
        }
    }
    Ok(())
}
