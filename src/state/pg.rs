//! Postgres: the pool, its TLS, and the ladder runner that brings each
//! store's schema up to date.
//!
//! `DATABASE_URL` carries a password, so nothing here logs or returns it:
//! boot logs name the host, port and database, and every error is walked
//! through its causes and then scrubbed of the URL and the password.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio_postgres::config::{Host, SslMode};

/// Connections one process holds, unless `TOOLSITE_DATABASE_POOL` says.
pub const DEFAULT_POOL_SIZE: usize = 16;

/// Advisory lock classes: the first half of a two-part key, one per
/// purpose, so two purposes never wait on each other by accident. The second
/// half names the object, or is 0 when the purpose has none.
pub const LOCK_MIGRATE: i32 = 1;

/// One store's schema, as the steps that build it. Version `n` is
/// `steps[n - 1]`; a step never changes once released, the ladder only grows.
pub struct Ladder {
    /// The Postgres schema the steps write to, and the name in
    /// `state.migrations`.
    pub store: &'static str,
    pub steps: &'static [&'static str],
}

/// Every ladder this build knows, applied at boot in this order.
pub const LADDERS: &[Ladder] = &[Ladder {
    store: "state",
    steps: &[
        include_str!("../../migrations/postgres/state/001_initial.sql"),
        include_str!("../../migrations/postgres/state/002_runner_placement.sql"),
    ],
}];

pub struct Postgres {
    pub pool: Pool,
    /// Host, port and database, for logs. Never the credentials.
    pub target: String,
    /// How the connection is protected, for logs.
    pub tls: &'static str,
}

/// How a connection is encrypted, from `sslmode` as libpq reads it.
#[derive(Debug, PartialEq)]
enum Tls {
    /// `disable`.
    Off,
    /// `prefer` (the default) and `require`: encrypted when the server
    /// offers it, or always, but the certificate is not checked. libpq does
    /// the same, and hosted Postgres often serves a self-signed certificate.
    EncryptOnly,
    /// `verify-ca` and `verify-full`: the certificate must chain to a root
    /// (`sslrootcert`, else the bundled web roots) and name the host.
    Verify(Option<PathBuf>),
}

struct Target {
    config: tokio_postgres::Config,
    tls: Tls,
    /// Every form the credentials could appear in, for `scrub`.
    secrets: Vec<String>,
}

/// Reads `DATABASE_URL`, in URL form or libpq's `key=value` form. Errors
/// never quote it.
fn parse(url: &str) -> Result<Target, String> {
    let url = url.trim();
    let mut secrets = vec![url.to_string()];
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        let config = tokio_postgres::Config::from_str(url)
            .map_err(|e| scrub(&format!("DATABASE_URL could not be read: {}", chain(&e)), &secrets))?;
        if let Some(password) = config.get_password() {
            secrets.push(String::from_utf8_lossy(password).into_owned());
        }
        let tls = if config.get_ssl_mode() == SslMode::Disable { Tls::Off } else { Tls::EncryptOnly };
        return Ok(Target { config, tls, secrets });
    }

    let mut parsed = url::Url::parse(url).map_err(|_| "DATABASE_URL is not a valid URL".to_string())?;
    if let Some(password) = parsed.password() {
        secrets.push(password.to_string());
        if let Ok(decoded) = urlencoding::decode(password) {
            secrets.push(decoded.into_owned());
        }
    }
    // tokio-postgres knows disable, prefer and require; the verifying modes
    // and the root file are libpq's, so they are taken out and handled here.
    let mut mode = "prefer".to_string();
    let mut root = None;
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter_map(|(key, value)| match key.as_ref() {
            "sslmode" => {
                mode = value.into_owned();
                None
            }
            "sslrootcert" => {
                root = Some(PathBuf::from(value.as_ref()));
                None
            }
            _ => Some((key.into_owned(), value.into_owned())),
        })
        .collect();
    parsed.set_query(None);
    if !kept.is_empty() {
        parsed.query_pairs_mut().extend_pairs(kept);
    }
    let (tls, ssl_mode) = match mode.as_str() {
        "disable" => (Tls::Off, SslMode::Disable),
        "allow" | "prefer" => (Tls::EncryptOnly, SslMode::Prefer),
        "require" => (Tls::EncryptOnly, SslMode::Require),
        "verify-ca" | "verify-full" => (Tls::Verify(root), SslMode::Require),
        other => return Err(format!("DATABASE_URL has sslmode={other}; use disable, prefer, require or verify-full")),
    };
    let mut config = tokio_postgres::Config::from_str(parsed.as_str())
        .map_err(|e| scrub(&format!("DATABASE_URL could not be read: {}", chain(&e)), &secrets))?;
    config.ssl_mode(ssl_mode);
    Ok(Target { config, tls, secrets })
}

/// Host, port and database: enough to tell which database a runner uses,
/// nothing that lets anyone in.
fn describe(config: &tokio_postgres::Config) -> String {
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| {
            let port = config.get_ports().get(i).or(config.get_ports().first()).copied().unwrap_or(5432);
            match host {
                Host::Tcp(name) => format!("{name}:{port}"),
                #[cfg(unix)]
                Host::Unix(path) => format!("{}:{port}", path.display()),
            }
        })
        .collect();
    let database = config.get_dbname().or(config.get_user()).unwrap_or("<default>");
    format!("{}/{database}", hosts.join(","))
}

/// An error and every cause under it. `Display` alone on a driver or pool
/// error prints the kind ("error connecting to server") and drops the reason.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let next = cause.to_string();
        if !text.contains(&next) {
            text.push_str(": ");
            text.push_str(&next);
        }
        source = cause.source();
    }
    text
}

/// Text with every form of the credentials taken out, longest first so a
/// password inside the URL goes with the URL.
fn scrub(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&String> = secrets.iter().filter(|s| !s.is_empty()).collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.iter().fold(text.to_string(), |text, secret| text.replace(secret.as_str(), "***"))
}

/// Builds the pool and proves one connection, so a wrong URL is a boot
/// error rather than every request failing later.
pub async fn connect(url: &str, pool_size: usize) -> Result<Postgres, String> {
    let Target { mut config, tls, secrets } = parse(url)?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(Duration::from_secs(10));
    }
    let target = describe(&config);
    let label = match (&tls, config.get_ssl_mode()) {
        (Tls::Off, _) => "off",
        (Tls::Verify(_), _) => "required, certificate verified",
        (Tls::EncryptOnly, SslMode::Prefer) => "if the server offers it, certificate not verified",
        (Tls::EncryptOnly, _) => "required, certificate not verified",
    };
    let connector = tokio_postgres_rustls::MakeRustlsConnect::new(client_config(&tls)?);
    let manager = Manager::from_config(config, connector, ManagerConfig { recycling_method: RecyclingMethod::Fast });
    let pool = Pool::builder(manager)
        .max_size(pool_size)
        .runtime(Runtime::Tokio1)
        .create_timeout(Some(Duration::from_secs(15)))
        .wait_timeout(Some(Duration::from_secs(30)))
        .build()
        .map_err(|e| scrub(&format!("could not build the Postgres pool: {}", chain(&e)), &secrets))?;
    drop(
        pool.get()
            .await
            .map_err(|e| scrub(&format!("could not connect to Postgres at {target}: {}", chain(&e)), &secrets))?,
    );
    Ok(Postgres { pool, target, tls: label })
}

fn client_config(tls: &Tls) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup: {e}"))?;
    Ok(match tls {
        Tls::Verify(root) => {
            let mut roots = rustls::RootCertStore::empty();
            match root {
                Some(path) => {
                    let certificates = CertificateDer::pem_file_iter(path)
                        .map_err(|e| format!("sslrootcert {} could not be read: {e}", path.display()))?;
                    for certificate in certificates {
                        let certificate =
                            certificate.map_err(|e| format!("sslrootcert {} is not PEM: {e}", path.display()))?;
                        roots
                            .add(certificate)
                            .map_err(|e| format!("sslrootcert {}: {e}", path.display()))?;
                    }
                }
                None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        Tls::Off | Tls::EncryptOnly => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(EncryptOnly(provider)))
            .with_no_client_auth(),
    })
}

/// libpq's `sslmode=require`: the channel is encrypted and the handshake's
/// signatures are checked, but any certificate is accepted.
#[derive(Debug)]
struct EncryptOnly(Arc<CryptoProvider>);

impl ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Applies every pending step of every ladder, each in its own transaction,
/// while holding the migration lock: runners booting together queue on it,
/// and each one after the first finds nothing left to do. Returns the steps
/// this call applied, as `store/version`.
///
/// Refuses a database that holds a version, or a store, this build does not
/// know: an older binary must not write to a newer schema.
pub async fn migrate(pool: &Pool, ladders: &[Ladder]) -> Result<Vec<String>, String> {
    let mut client = pool.get().await.map_err(|e| format!("could not reach Postgres to migrate: {}", chain(&e)))?;
    client
        .execute("select pg_advisory_lock($1::int4, 0)", &[&LOCK_MIGRATE])
        .await
        .map_err(|e| format!("could not take the migration lock: {}", chain(&e)))?;
    let climbed = climb(&mut client, ladders).await;
    if client
        .execute("select pg_advisory_unlock($1::int4, 0)", &[&LOCK_MIGRATE])
        .await
        .is_err()
    {
        // A session lock lives as long as its connection: closing this one
        // instead of returning it to the pool is what lets the next runner in.
        drop(deadpool_postgres::Object::take(client));
    }
    climbed
}

async fn climb(client: &mut deadpool_postgres::Client, ladders: &[Ladder]) -> Result<Vec<String>, String> {
    client
        .batch_execute(
            "create schema if not exists state;
             create table if not exists state.migrations (
                 store text not null,
                 version integer not null,
                 applied_at bigint not null,
                 primary key (store, version)
             );",
        )
        .await
        .map_err(|e| format!("could not create state.migrations: {}", chain(&e)))?;

    let rows = client
        .query("select store, max(version) from state.migrations group by store", &[])
        .await
        .map_err(|e| format!("could not read state.migrations: {}", chain(&e)))?;
    let mut have = std::collections::HashMap::new();
    for row in rows {
        let store: String = row.get(0);
        let version: i32 = row.get(1);
        match ladders.iter().find(|ladder| ladder.store == store) {
            None => {
                return Err(format!(
                    "the database has a schema `{store}` this build does not know: it was migrated by a \
                     newer toolsite, so run that version or newer"
                ));
            }
            Some(ladder) if version as usize > ladder.steps.len() => {
                return Err(format!(
                    "the database's `{store}` schema is at version {version}, past the {} this build \
                     knows: it was migrated by a newer toolsite, so run that version or newer",
                    ladder.steps.len()
                ));
            }
            Some(_) => {
                have.insert(store, version as usize);
            }
        }
    }

    let mut applied = Vec::new();
    for ladder in ladders {
        let done = have.get(ladder.store).copied().unwrap_or(0);
        for (index, step) in ladder.steps.iter().enumerate().skip(done) {
            let version = index as i32 + 1;
            let name = format!("{}/{version:03}", ladder.store);
            let transaction = client
                .transaction()
                .await
                .map_err(|e| format!("could not begin {name}: {}", chain(&e)))?;
            transaction
                .batch_execute(step)
                .await
                .map_err(|e| format!("migration {name} failed: {}", chain(&e)))?;
            transaction
                .execute(
                    "insert into state.migrations (store, version, applied_at) values ($1, $2, $3)",
                    &[&ladder.store, &version, &now()],
                )
                .await
                .map_err(|e| format!("could not record {name}: {}", chain(&e)))?;
            transaction
                .commit()
                .await
                .map_err(|e| format!("could not commit {name}: {}", chain(&e)))?;
            applied.push(name);
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_of_the_database_carries_no_credentials() {
        let target = parse("postgres://toolsite:hunter2%40x@db.internal:6543/site?application_name=ts").unwrap();
        let described = describe(&target.config);
        assert_eq!(described, "db.internal:6543/site");
        assert!(!described.contains("hunter2") && !described.contains("toolsite:"));
    }

    #[test]
    fn scrubbing_takes_out_the_url_and_both_forms_of_the_password() {
        let url = "postgres://toolsite:p%40ss-w0rd@db.internal/site";
        let target = parse(url).unwrap();
        let logged = scrub(
            &format!("failed for {url}; password p@ss-w0rd or p%40ss-w0rd was refused"),
            &target.secrets,
        );
        assert!(!logged.contains("p@ss-w0rd") && !logged.contains("p%40ss-w0rd"), "{logged}");
        assert!(!logged.contains("postgres://"), "{logged}");
    }

    #[test]
    fn a_key_value_url_is_scrubbed_too() {
        let target = parse("host=db.internal user=toolsite password=hunter2 dbname=site").unwrap();
        assert_eq!(describe(&target.config), "db.internal:5432/site");
        assert!(!scrub("auth failed with hunter2", &target.secrets).contains("hunter2"));
    }

    #[test]
    fn a_url_that_does_not_parse_is_not_quoted_back() {
        for url in ["postgres://toolsite:hunter2@[bad", "host=db password=hunter2 port=notaport"] {
            let why = parse(url).err().expect("parsed");
            assert!(!why.contains("hunter2"), "{why}");
        }
    }

    #[test]
    fn sslmode_is_read_as_libpq_reads_it() {
        let mode = |url: &str| parse(url).map(|t| (t.tls, t.config.get_ssl_mode()));
        assert_eq!(mode("postgres://u@h/d").unwrap(), (Tls::EncryptOnly, SslMode::Prefer));
        assert_eq!(mode("postgres://u@h/d?sslmode=disable").unwrap(), (Tls::Off, SslMode::Disable));
        assert_eq!(mode("postgres://u@h/d?sslmode=require").unwrap(), (Tls::EncryptOnly, SslMode::Require));
        assert_eq!(mode("postgres://u@h/d?sslmode=verify-full").unwrap(), (Tls::Verify(None), SslMode::Require));
        assert_eq!(
            mode("postgres://u@h/d?sslmode=verify-ca&sslrootcert=/etc/ca.pem&application_name=x").unwrap(),
            (Tls::Verify(Some(PathBuf::from("/etc/ca.pem"))), SslMode::Require)
        );
        assert!(mode("postgres://u@h/d?sslmode=sometimes").is_err());
    }

    #[test]
    fn every_tls_mode_builds_a_client() {
        client_config(&Tls::Off).unwrap();
        client_config(&Tls::EncryptOnly).unwrap();
        client_config(&Tls::Verify(None)).unwrap();
        assert!(client_config(&Tls::Verify(Some(PathBuf::from("/nonexistent/ca.pem")))).is_err());
    }

    #[tokio::test]
    async fn a_failed_connection_reports_why_without_the_password() {
        // Nothing listens on port 1, so the connect fails at once.
        let why = connect("postgres://toolsite:hunter2-s3cret@127.0.0.1:1/site?sslmode=disable", 1)
            .await
            .err()
            .expect("connected to port 1");
        assert!(!why.contains("hunter2-s3cret"), "{why}");
        assert!(why.contains("127.0.0.1:1/site"), "{why}");
        // The cause, not just the kind.
        assert!(why.to_lowercase().contains("refused"), "{why}");
    }

    #[test]
    fn every_ladder_is_named_once_and_every_step_has_sql() {
        let mut stores: Vec<&str> = LADDERS.iter().map(|l| l.store).collect();
        stores.sort();
        stores.dedup();
        assert_eq!(stores.len(), LADDERS.len());
        assert!(LADDERS.iter().all(|l| l.steps.iter().all(|s| !s.trim().is_empty())));
    }
}
