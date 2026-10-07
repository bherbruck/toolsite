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
    let asked = read(&["TOOLSITE_DEFAULT_ACCESS"]).unwrap_or_else(|| "public".into());
    let default_gate = match toolsite::content::store::normalise_gate(&asked) {
        Some(level) => level.to_string(),
        None => panic!(
            "TOOLSITE_DEFAULT_ACCESS must be one of {} (granted is the old name for restricted), not {asked:?}",
            toolsite::content::store::GATES.join(", ")
        ),
    };

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

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("0.0.0.0:{port}");

    // Screenshots: a sidecar named by TOOLSITE_BROWSER_URL, else a browser in
    // this container, else none. Chosen once, here, so a missing one is a
    // boot log line rather than a tool failure later.
    let renderer = toolsite::platform::screenshot::from_env().unwrap_or_else(|why| panic!("{why}"));
    let preview_base = toolsite::platform::screenshot::preview_base_from_env(&format!("http://127.0.0.1:{port}"))
        .unwrap_or_else(|why| panic!("{why}"));
    match &renderer {
        Some(renderer) => tracing::info!(renderer = %renderer.describe(), preview_base = %preview_base, "screenshots available"),
        None => tracing::info!("screenshots unavailable: set TOOLSITE_BROWSER_URL for a sidecar or TOOLSITE_BROWSER for a local browser"),
    }

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

    // Live connections: how many sockets an app and a person may hold, and
    // how fast an app may send. Whole numbers; the defaults suit one box.
    let count = |name: &str, default: u64| -> u64 {
        match read(&[name]) {
            Some(value) => value
                .trim()
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{name} must be a whole number, not {value:?}")),
            None => default,
        }
    };
    let socket_defaults = toolsite::runtime::connections::Limits::default();
    let socket_limits = toolsite::runtime::connections::Limits {
        per_app: count("TOOLSITE_SOCKETS_PER_APP", socket_defaults.per_app as u64) as usize,
        per_person: count("TOOLSITE_SOCKETS_PER_PERSON", socket_defaults.per_person as u64) as usize,
        total: count("TOOLSITE_SOCKETS_TOTAL", socket_defaults.total as u64) as usize,
        rate_per_app: count("TOOLSITE_SOCKET_MESSAGES_PER_SECOND", socket_defaults.rate_per_app as u64) as u32,
        check_every: socket_defaults.check_every,
        raw_per_app: count("TOOLSITE_TCP_PER_APP", socket_defaults.raw_per_app as u64) as usize,
        per_ip: count("TOOLSITE_TCP_PER_IP", socket_defaults.per_ip as u64) as usize,
        tcp_idle: std::time::Duration::from_secs(count("TOOLSITE_TCP_IDLE_SECONDS", socket_defaults.tcp_idle.as_secs())),
        udp_idle: std::time::Duration::from_secs(count("TOOLSITE_UDP_IDLE_SECONDS", socket_defaults.udp_idle.as_secs())),
        udp_per_second: count("TOOLSITE_UDP_PER_SECOND", socket_defaults.udp_per_second as u64) as u32,
        udp_queued_bytes: count("TOOLSITE_UDP_QUEUED_BYTES", socket_defaults.udp_queued_bytes as u64) as usize,
        tcp_send_timeout: std::time::Duration::from_secs(count(
            "TOOLSITE_TCP_SEND_SECONDS",
            socket_defaults.tcp_send_timeout.as_secs(),
        )),
    };
    tracing::info!(
        per_app = socket_limits.per_app,
        per_person = socket_limits.per_person,
        total = socket_limits.total,
        rate_per_app = socket_limits.rate_per_app,
        tcp_per_app = socket_limits.raw_per_app,
        tcp_per_ip = socket_limits.per_ip,
        tcp_idle_seconds = socket_limits.tcp_idle.as_secs(),
        udp_idle_seconds = socket_limits.udp_idle.as_secs(),
        udp_per_second = socket_limits.udp_per_second,
        udp_queued_bytes = socket_limits.udp_queued_bytes,
        tcp_send_seconds = socket_limits.tcp_send_timeout.as_secs(),
        "live connections configuration"
    );

    // Resident apps: the memory one gets when its manifest does not say,
    // and the most one may ask for.
    let resident_defaults = toolsite::runtime::resident::Residents::default();
    let resident_max = count("TOOLSITE_RESIDENT_MAX_MB", resident_defaults.max_memory_mb);
    let resident_memory = count("TOOLSITE_RESIDENT_MEMORY_MB", resident_defaults.default_memory_mb);
    if resident_memory > resident_max {
        panic!("TOOLSITE_RESIDENT_MEMORY_MB is {resident_memory}, past TOOLSITE_RESIDENT_MAX_MB of {resident_max}");
    }
    let mut residents = toolsite::runtime::resident::Residents::new(resident_memory, resident_max);
    // And what all of them together may take: threads, memory, waiting events.
    residents.max_instances = count("TOOLSITE_RESIDENT_MAX", residents.max_instances as u64) as usize;
    residents.total_memory_mb = count("TOOLSITE_RESIDENT_TOTAL_MB", residents.total_memory_mb);
    residents.queue_depth = count("TOOLSITE_RESIDENT_QUEUE", residents.queue_depth as u64).max(1) as usize;
    if resident_max > residents.total_memory_mb {
        panic!(
            "TOOLSITE_RESIDENT_MAX_MB is {resident_max}, past TOOLSITE_RESIDENT_TOTAL_MB of {}",
            residents.total_memory_mb
        );
    }
    tracing::info!(
        memory_mb = resident_memory,
        max_mb = resident_max,
        instances = residents.max_instances,
        total_mb = residents.total_memory_mb,
        queue = residents.queue_depth,
        "resident apps configuration"
    );

    // TCP and UDP ports beyond HTTP, each given to one app by the site's
    // owner: `1883=mqtt-broker,5514/udp=syslog`. A bad map is a startup
    // error, not a device that cannot connect.
    let port_map = toolsite::platform::ports::PortMap {
        mappings: toolsite::platform::ports::parse(&read(&["TOOLSITE_PORTS"]).unwrap_or_default())
            .unwrap_or_else(|why| panic!("{why}")),
        ..Default::default()
    };
    if let Some(clash) = port_map
        .mappings
        .iter()
        .find(|m| m.socket.protocol == toolsite::content::store::PortProtocol::Tcp && m.socket.port.to_string() == port)
    {
        panic!("TOOLSITE_PORTS maps {} to {}, but that is PORT, where HTTP is served", clash.socket, clash.app);
    }

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
        renderer,
        preview_base,
        connections: Arc::new(toolsite::runtime::connections::Hub::new(socket_limits)),
        ports: port_map,
        residents: Arc::new(residents),
    });

    // Per-app grants became View rows on their apps; done once.
    toolsite::platform::permissions::adopt_grants(&config).await;

    // A project move that stopped halfway, say with the process, is finished
    // before anything is served, so no request sees the state in between.
    if let Err(why) = toolsite::platform::projects::resume_pending(&config).await {
        tracing::error!(%why, "a project move could not be finished; check .site/relocating.json");
    }

    let runtime = Runtime::new()?;
    let app = build_router(config.clone(), runtime.clone());
    // One scheduler for the process, started here rather than with a
    // router: anything that builds a second router would fire every job twice.
    toolsite::platform::schedule::Scheduler::new(toolsite::AppState {
        config: config.clone(),
        runtime: runtime.clone(),
    })
    .spawn();

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on {addr}");
    toolsite::platform::ports::listen(config.clone(), runtime.clone())
        .await
        .unwrap_or_else(|why| panic!("a port in TOOLSITE_PORTS could not be opened: {why}"));

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
        renderer: None,
        preview_base: "http://127.0.0.1:8080".to_string(),
        connections: Arc::new(toolsite::runtime::connections::Hub::default()),
        ports: Default::default(),
        residents: Default::default(),
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
