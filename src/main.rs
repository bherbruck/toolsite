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
    platform::mcp::PageHost,
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

    // Authenticates MCP *clients* — who may publish — and nothing else. An
    // OAuth client signs in with an admin account instead, which needs no
    // variable beyond the base URL.
    let bearer_token = read(&[
        "TOOLSITE_MCP_TOKEN",
        "TOOLSITE_TOKEN",
        "BEARER_TOKEN",
        "MCP_TOKEN",
    ]);
    // The client-id/secret shim these used to configure is gone: clients sign
    // in now. The secret was also the access token it handed out, so it stays
    // valid as a plain bearer until every connector has reconnected.
    let legacy_oauth_secret = read(&[
        "TOOLSITE_MCP_OAUTH_CLIENT_SECRET",
        "TOOLSITE_OAUTH_CLIENT_SECRET",
        "OAUTH_CLIENT_SECRET",
    ]);
    if legacy_oauth_secret.is_some() {
        tracing::warn!(
            "TOOLSITE_MCP_OAUTH_CLIENT_SECRET is no longer an OAuth setting: clients now \
             sign in with an admin account. The secret still works as a bearer token; \
             reconnect your clients and then drop it, along with the client id"
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
    // Over stdio the client already owns the process, so there is nothing for
    // a token to protect; HTTP still refuses everything without one, and the
    // OAuth server cannot issue any until it knows its own address.
    if !stdio && bearer_token.is_none() && base_url.is_none() {
        panic!(
            "set TOOLSITE_MCP_TOKEN for static-token clients, or TOOLSITE_BASE_URL so \
             clients can sign in with an admin account (or both)"
        );
    }

    let mut valid_tokens = Vec::new();
    if let Some(t) = &bearer_token {
        valid_tokens.push(t.clone());
    }
    if let Some(secret) = &legacy_oauth_secret {
        valid_tokens.push(secret.clone());
    }

    // Ceilings, in megabytes. Zero lifts one entirely.
    let megabytes = |names: &[&str], default: u64| -> u64 {
        match read(names) {
            Some(value) => value
                .trim()
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{} must be a whole number of MB (0 for no limit), not {value:?}", names[0]))
                * 1024
                * 1024,
            None => default,
        }
    };
    let max_db_bytes = megabytes(&["TOOLSITE_MAX_DB_MB"], toolsite::config::DEFAULT_MAX_DB_BYTES);
    let max_blob_bytes = megabytes(&["TOOLSITE_MAX_BLOB_MB"], toolsite::config::DEFAULT_MAX_BLOB_BYTES);

    // Files go to a bucket when one is configured, else beside the app on
    // disk. The unprefixed names are what a Railway bucket injects by
    // reference, so pointing at one is five variable references and nothing
    // else.
    let blobs = match (
        read(&["TOOLSITE_BLOB_S3_ENDPOINT", "ENDPOINT"]),
        read(&["TOOLSITE_BLOB_S3_BUCKET", "BUCKET"]),
    ) {
        (Some(endpoint), Some(bucket)) => {
            let access_key_id = read(&["TOOLSITE_BLOB_S3_ACCESS_KEY_ID", "ACCESS_KEY_ID"])
                .expect("a blob bucket needs TOOLSITE_BLOB_S3_ACCESS_KEY_ID");
            let secret = read(&["TOOLSITE_BLOB_S3_SECRET_ACCESS_KEY", "SECRET_ACCESS_KEY"])
                .expect("a blob bucket needs TOOLSITE_BLOB_S3_SECRET_ACCESS_KEY");
            let region = read(&["TOOLSITE_BLOB_S3_REGION", "REGION"]).unwrap_or_else(|| "auto".into());
            let path_style = read(&["TOOLSITE_BLOB_S3_PATH_STYLE"]).is_some_and(|v| v != "0");
            let s3 = toolsite::runtime::blobs::S3::new(
                &endpoint, &bucket, &region, &access_key_id, &secret, path_style,
            )
            .unwrap_or_else(|why| panic!("{why}"));
            toolsite::runtime::blobs::Blobs {
                backend: toolsite::runtime::blobs::Backend::S3(s3),
                max_bytes: max_blob_bytes,
            }
        }
        (None, None) => toolsite::runtime::blobs::Blobs::local(max_blob_bytes),
        _ => panic!("set TOOLSITE_BLOB_S3_ENDPOINT and TOOLSITE_BLOB_S3_BUCKET together, or neither"),
    };

    // Ways to sign in besides a password: TOOLSITE_LOGIN_<SLUG>_CLIENT_ID and
    // friends, one group per provider. A broken group is a startup error,
    // not a button that fails when someone clicks it.
    let providers = toolsite::accounts::providers::from_env(std::env::vars())
        .unwrap_or_else(|why| panic!("{why}"));
    if base_url.is_none() && !providers.is_empty() {
        panic!("TOOLSITE_BASE_URL is required with TOOLSITE_LOGIN_* (the provider sends people back to it)");
    }
    for provider in &providers {
        tracing::info!(
            provider = %provider.slug,
            name = %provider.name,
            kind = provider.kind_name(),
            allow_domain = provider.allow_domain.as_deref().unwrap_or("<none>"),
            "sign-in provider"
        );
    }

    // What an app is until it says otherwise. An internal deployment sets
    // this to granted or authenticated once and never thinks about it again.
    let default_gate = read(&["TOOLSITE_DEFAULT_ACCESS"]).unwrap_or_else(|| "public".into());
    if !toolsite::content::store::GATES.contains(&default_gate.as_str()) {
        panic!(
            "TOOLSITE_DEFAULT_ACCESS must be one of {}, not {default_gate:?}",
            toolsite::content::store::GATES.join(", ")
        );
    }

    // A GitHub App, when there is one, so apps can live in repositories and
    // deploy from them. The id and key are the App; the rest is optional.
    let github = match (
        read(&["TOOLSITE_GITHUB_APP_ID"]),
        read(&["TOOLSITE_GITHUB_APP_PRIVATE_KEY"]),
    ) {
        (Some(id), Some(key)) => Some(
            toolsite::platform::github::App::new(
                &id,
                &key,
                read(&["TOOLSITE_GITHUB_APP_SLUG"]),
                read(&["TOOLSITE_GITHUB_WEBHOOK_SECRET"]),
                read(&["TOOLSITE_GITHUB_API"]),
            )
            .unwrap_or_else(|why| panic!("{why}")),
        ),
        (None, None) => None,
        _ => panic!("set TOOLSITE_GITHUB_APP_ID and TOOLSITE_GITHUB_APP_PRIVATE_KEY together, or neither"),
    };
    if let Some(app) = &github {
        if base_url.is_none() {
            panic!("TOOLSITE_BASE_URL is required with TOOLSITE_GITHUB_*: the README toolsite writes into a repository names the site");
        }
        tracing::info!(app_id = %app.app_id, webhook = app.install_url().is_some(), "github app configured");
    }

    // Screenshots need a browser on the machine. Found once, here, so a
    // missing one is a boot log line rather than a tool failure later.
    let browser = toolsite::platform::screenshot::find_browser();
    match &browser {
        Some(path) => tracing::info!(browser = %path.display(), "screenshots available"),
        None => tracing::info!("screenshots unavailable: no browser found (TOOLSITE_BROWSER)"),
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    // Printed at boot so a misconfigured deploy is obvious from the logs
    // rather than only from a client's opaque "can't connect".
    tracing::info!(
        bearer_auth = bearer_token.is_some(),
        oauth_auth = base_url.is_some(),
        base_url = base_url.as_deref().unwrap_or("<unset>"),
        default_access = %default_gate,
        "auth configuration"
    );
    tracing::info!(
        blobs = blobs.describe(),
        max_db_mb = max_db_bytes / 1024 / 1024,
        max_blob_mb = max_blob_bytes / 1024 / 1024,
        "storage configuration (0 MB means no ceiling)"
    );

    let config = Arc::new(Config {
        data_dir,
        base_url,
        local_base: format!("http://localhost:{port}"),
        valid_tokens,
        uploads: Mutex::new(HashMap::new()),
        inline_uploads: Mutex::new(HashMap::new()),
        max_db_bytes,
        blobs,
        blob_uploads: Mutex::new(HashMap::new()),
        providers,
        logins: Mutex::new(HashMap::new()),
        default_gate,
        github,
        previews: Mutex::new(HashMap::new()),
        browser,
    });

    let runtime = Runtime::new()?;
    let app = build_router(config.clone(), runtime.clone());

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
    let service = PageHost::new(config, runtime)
        .serve(rmcp::transport::stdio())
        .await?;
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
        uploads: Mutex::new(HashMap::new()),
        inline_uploads: Mutex::new(HashMap::new()),
        max_db_bytes: toolsite::config::DEFAULT_MAX_DB_BYTES,
        blobs: toolsite::runtime::blobs::Blobs::local(toolsite::config::DEFAULT_MAX_BLOB_BYTES),
        blob_uploads: Mutex::new(HashMap::new()),
        providers: Vec::new(),
        logins: Mutex::new(HashMap::new()),
        default_gate: "public".to_string(),
        github: None,
        previews: Mutex::new(HashMap::new()),
        browser: None,
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
