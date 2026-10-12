use std::{
    io::{self, IsTerminal},
    net::SocketAddr,
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use trama_collab::{
    AuthConfig, AuthService, MAX_PASSWORD_BYTES, read_secret, router, validate_bind,
};

#[derive(Parser)]
#[command(about = "Local development collaboration service for TramaWorkspace")]
struct Args {
    #[arg(long, global = true, default_value = "trama-auth.sqlite")]
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
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: SocketAddr,
    },
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
            let auth = AuthService::open(args.db, AuthConfig::default())?;
            auth.create_user(&username, &password).await?;
            println!("Created user {username}");
        }
        Command::CreateRunner { username, name } => {
            let auth = AuthService::open(args.db, AuthConfig::default())?;
            let token = auth.create_runner(&username, &name).await?;
            eprintln!(
                "Created runner {name} for {username}. Store this token now; it is not shown again:"
            );
            println!("{token}");
        }
        Command::Serve { bind } => {
            validate_bind(bind)?;
            let auth = AuthService::open(args.db, AuthConfig::default())?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            println!("Collaboration service listening on http://{bind}");
            axum::serve(
                listener,
                router(auth).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
        }
    }
    Ok(())
}
