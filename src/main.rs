use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::fs;
use rmcp::ServiceExt;
use toolsite::{
    build_router,
    config::Config,
    platform::{client_oauth::OAuth, mcp::PageHost},
    runtime::wasm::Runtime,
};

/// Run with no subcommand to serve. The subcommands exist for a shell on the
/// machine itself: they work directly on DATA_DIR, so bootstrapping the first
/// account needs no token and no network.
#[derive(clap::Parser)]
#[command(name = "toolsite", version)]
struct Cli {
    /// Speak MCP on stdin/stdout instead of waiting for HTTP.
    #[arg(long)]
    stdio: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Accounts, straight against this machine's data directory.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
}

#[derive(clap::Subcommand)]
enum UserCommand {
    /// Create an account. Prints a one-time link for choosing a password
    /// unless one is given here.
    Add {
        email: String,
        #[arg(long)]
        admin: bool,
        /// Skip the link and set the password now. Ends up in shell history.
        #[arg(long)]
        password: Option<String>,
    },
    /// A fresh link for someone who lost theirs, or never set a password.
    Invite { email: String },
    /// Everyone, with their status.
    List,
    /// Stop an account signing in, and end its sessions now.
    Disable { email: String },
    /// Let a disabled account back in.
    Enable { email: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Local dev convenience; in a container the env is set directly.
    dotenvy::dotenv().ok();

    let cli = <Cli as clap::Parser>::parse();

    // Speaking MCP over stdio makes stdout the protocol channel, so every log
    // line has to go to stderr or it corrupts the stream.
    let stdio = cli.stdio || std::env::var("MCP_STDIO").is_ok_and(|v| v != "0");

    // Without this the default filter drops everything, so a deployed instance
    // looks silent even while it's rejecting requests.
    let logs = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    );
    if stdio {
        logs.with_writer(std::io::stderr).init();
    } else {
        logs.init();
    }

    // Ours are prefixed so they cannot collide with anything else in a shared
    // environment; the first name is current and the rest are kept working.
    // PORT and RUST_LOG stay unprefixed on purpose — the platform injects the
    // one and the Rust ecosystem owns the other.
    let read = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    };

    let data_dir = PathBuf::from(
        read(&["TOOLSITE_DATA_DIR", "DATA_DIR"]).unwrap_or_else(|| "/data".into()),
    );
    fs::create_dir_all(&data_dir).await?;

    if let Some(Command::User { command }) = cli.command {
        return run_user_command(command, data_dir, read(&["TOOLSITE_BASE_URL", "PUBLIC_BASE_URL"]));
    }

    let bearer_token = read(&["TOOLSITE_TOKEN", "BEARER_TOKEN", "MCP_TOKEN"]);
    let oauth_client_id = read(&["TOOLSITE_OAUTH_CLIENT_ID", "OAUTH_CLIENT_ID"]);
    let oauth_client_secret = read(&["TOOLSITE_OAUTH_CLIENT_SECRET", "OAUTH_CLIENT_SECRET"]);

    let oauth = match (oauth_client_id, oauth_client_secret) {
        (Some(client_id), Some(client_secret)) => Some(OAuth {
            client_id,
            client_secret,
            auth_codes: Mutex::new(HashMap::new()),
        }),
        (None, None) => None,
        _ => panic!("set both OAUTH_CLIENT_ID and OAUTH_CLIENT_SECRET together, or neither"),
    };

    // Over stdio the client already owns the process, so there is nothing for
    // a token to protect; HTTP still refuses everything without one.
    if !stdio && bearer_token.is_none() && oauth.is_none() {
        panic!(
            "set TOOLSITE_TOKEN, or TOOLSITE_OAUTH_CLIENT_ID + \
             TOOLSITE_OAUTH_CLIENT_SECRET (or both)"
        );
    }

    // A bare host is the natural thing to paste in, but every URL built from
    // this needs a scheme to be usable, so supply one rather than emitting
    // href-less strings like "example.com/p/slug".
    let base_url = read(&["TOOLSITE_BASE_URL", "PUBLIC_BASE_URL"])
        // Quotes survive a copy-paste into a dashboard field, and would
        // otherwise end up inside every URL this server hands out.
        .map(|s| s.trim().trim_matches('"').trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.contains("://") {
                s
            } else {
                format!("https://{s}")
            }
        });
    if oauth.is_some() && base_url.is_none() {
        panic!(
            "TOOLSITE_BASE_URL is required alongside the OAuth variables \
             (discovery metadata needs absolute URLs)"
        );
    }

    let mut valid_tokens = Vec::new();
    if let Some(t) = &bearer_token {
        valid_tokens.push(t.clone());
    }
    if let Some(o) = &oauth {
        valid_tokens.push(o.client_secret.clone());
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    // Printed at boot so a misconfigured deploy is obvious from the logs
    // rather than only from a client's opaque "can't connect".
    tracing::info!(
        bearer_auth = bearer_token.is_some(),
        oauth_auth = oauth.is_some(),
        base_url = base_url.as_deref().unwrap_or("<unset>"),
        "auth configuration"
    );

    let config = Arc::new(Config {
        data_dir,
        base_url,
        local_base: format!("http://localhost:{port}"),
        valid_tokens,
        oauth,
        uploads: Mutex::new(HashMap::new()),
    });

    let app = build_router(config.clone(), Runtime::new()?);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");

    if !stdio {
        axum::serve(listener, app).await?;
        return Ok(());
    }

    // The web server keeps running alongside: an agent talks MCP over stdio
    // but still needs somewhere to curl uploads to, and somewhere to view the
    // published page.
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(%error, "web server stopped");
        }
    });

    tracing::info!("serving MCP on stdio");
    let service = PageHost::new(config).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Account management from a shell on the machine. No token, no HTTP — it
/// opens the account database the same way the server does.
fn run_user_command(
    command: UserCommand,
    data_dir: PathBuf,
    base_url: Option<String>,
) -> anyhow::Result<()> {
    use toolsite::accounts::users;

    let config = Config {
        data_dir,
        base_url: base_url.map(|url| {
            let url = url.trim().trim_matches('"').trim_end_matches('/').to_string();
            if url.contains("://") {
                url
            } else {
                format!("https://{url}")
            }
        }),
        local_base: "http://localhost:8080".to_string(),
        valid_tokens: Vec::new(),
        oauth: None,
        uploads: Mutex::new(HashMap::new()),
    };

    let report = |result: Result<(), String>, done: &str| -> anyhow::Result<()> {
        match result {
            Ok(()) => {
                println!("{done}");
                Ok(())
            }
            Err(message) => anyhow::bail!(message),
        }
    };

    match command {
        UserCommand::Add {
            email,
            admin,
            password: Some(password),
        } => {
            let user = users::sign_up_as(&config, &email, &password, admin)
                .map_err(anyhow::Error::msg)?;
            println!(
                "created {}{}",
                user.email,
                if user.is_admin { " as an admin" } else { "" }
            );
            Ok(())
        }
        UserCommand::Add {
            email,
            admin,
            password: None,
        } => {
            let (user, token) =
                users::invite(&config, &email, admin).map_err(anyhow::Error::msg)?;
            println!(
                "created {}{}\n\nOpen this to choose a password (48 hours, one use):\n{}",
                user.email,
                if user.is_admin { " as an admin" } else { "" },
                users::invite_url(&config, &token)
            );
            Ok(())
        }
        UserCommand::Invite { email } => {
            let token = users::reinvite(&config, &email).map_err(anyhow::Error::msg)?;
            println!("{}", users::invite_url(&config, &token));
            Ok(())
        }
        UserCommand::List => {
            let accounts = users::list_accounts(&config).map_err(anyhow::Error::msg)?;
            if accounts.is_empty() {
                println!("no accounts yet");
            }
            for account in accounts {
                println!(
                    "{:<36} {:<10} {}{}",
                    account.email,
                    if account.is_active { "active" } else { "disabled" },
                    if account.is_admin { "admin " } else { "" },
                    account.created
                );
            }
            Ok(())
        }
        UserCommand::Disable { email } => report(
            users::set_active(&config, &email, false),
            "disabled; its sessions are gone",
        ),
        UserCommand::Enable { email } => {
            report(users::set_active(&config, &email, true), "active again")
        }
    }
}
