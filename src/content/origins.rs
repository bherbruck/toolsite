//! Which origin serves an app.
//!
//! Path mode, the default: every app is at `<site>/p/<app>/`, on one origin
//! with every other app and with toolsite's own pages.
//!
//! Subdomain mode, on when `TOOLSITE_APPS_DOMAIN` is set: each app is served
//! from an origin of its own, `<label>.<apps domain>`, and still at the path
//! `/p/<app>/`, so a build made for path mode works as it is. The browser
//! then keeps each app's scripts, cookies and storage apart from every other
//! app's and from the main host's.
//!
//! A label is a DNS label chosen once per app and kept in its meta, so the
//! host does not change when the app moves to another project or another
//! app's name would now claim it. Every URL to an app is built here, from
//! the configuration and the stored label, and never from a request's
//! `Host` header.

use crate::{
    config::Config,
    content::{catalog, slug::valid_segment},
};
use sha2::{Digest, Sha256};

/// The longest DNS label.
pub const MAX_LABEL: usize = 63;

/// The domain apps are served under, and how they are reached.
#[derive(Debug, Clone, PartialEq)]
pub struct AppsDomain {
    /// Lower case, with no leading or trailing dot: `apps.example.com`.
    pub domain: String,
    /// `https` or `http`, the same as the main site's.
    pub scheme: String,
    /// A port other than the scheme's default, for a server reached on one.
    pub port: Option<u16>,
}

impl AppsDomain {
    /// From `TOOLSITE_APPS_DOMAIN`, with the scheme and port taken from the
    /// site's base URL unless `TOOLSITE_APPS_PORT` names a port.
    pub fn parse(domain: &str, base_url: Option<&str>, port: Option<&str>) -> Result<Self, String> {
        let domain = domain.trim().trim_matches('"').to_ascii_lowercase();
        let valid = !domain.is_empty()
            && domain.len() <= 200
            && domain.split('.').count() >= 2
            && domain.split('.').all(|part| {
                !part.is_empty()
                    && part.len() <= MAX_LABEL
                    && !part.starts_with('-')
                    && !part.ends_with('-')
                    && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if !valid {
            return Err(format!(
                "TOOLSITE_APPS_DOMAIN must be a domain such as apps.example.com, with no scheme, port or path, not {domain:?}"
            ));
        }
        let base = base_url.ok_or("TOOLSITE_APPS_DOMAIN needs TOOLSITE_BASE_URL, which says the scheme apps are served over")?;
        let (scheme, rest) = base.split_once("://").ok_or("TOOLSITE_BASE_URL has no scheme")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(format!("TOOLSITE_BASE_URL must be http or https, not {scheme}"));
        }
        let authority = rest.split('/').next().unwrap_or_default();
        let base_host = authority.rsplit_once(':').map_or(authority, |(host, _)| host).to_ascii_lowercase();
        if base_host == domain || base_host.ends_with(&format!(".{domain}")) {
            return Err(format!(
                "the main site {base_host} must not be under TOOLSITE_APPS_DOMAIN {domain}, or it would share cookies with apps"
            ));
        }
        let port = match port.map(str::trim).filter(|p| !p.is_empty()) {
            Some(port) => Some(port.parse::<u16>().map_err(|_| format!("TOOLSITE_APPS_PORT must be a port number, not {port:?}"))?),
            None => authority.rsplit_once(':').and_then(|(_, port)| port.parse::<u16>().ok()),
        };
        let port = port.filter(|p| !matches!((scheme.as_str(), *p), ("https", 443) | ("http", 80)));
        Ok(Self { domain, scheme, port })
    }

    /// `scheme://<label>.<domain>[:port]`.
    pub fn origin(&self, label: &str) -> String {
        match self.port {
            Some(port) => format!("{}://{label}.{}:{port}", self.scheme, self.domain),
            None => format!("{}://{label}.{}", self.scheme, self.domain),
        }
    }

    /// Whether a browser keeps a `Secure` cookie for an app host: over
    /// https, and on `localhost` names, which browsers treat as secure.
    pub fn secure(&self) -> bool {
        self.scheme == "https" || self.domain == "localhost" || self.domain.ends_with(".localhost")
    }

    /// The label of a host under this domain, from a `Host` header. A host
    /// with another port, a trailing dot, more than one label in front of
    /// the domain, or anything but `[a-z0-9-]` is not one of ours.
    fn label_of<'h>(&self, host: &'h str) -> Option<&'h str> {
        let name = match (host.rsplit_once(':'), self.port) {
            (Some((name, port)), Some(expected)) if port.parse::<u16>().ok() == Some(expected) => name,
            (Some(_), _) => return None,
            (None, Some(_)) => return None,
            (None, None) => host,
        };
        let label = name.strip_suffix(&self.domain)?.strip_suffix('.')?;
        valid_label(label).then_some(label)
    }
}

/// A DNS label as this module issues them: 1 to 63 of `[a-z0-9-]`, not
/// starting or ending with a hyphen.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether a label has hyphens in its third and fourth places, which IDNA
/// reserves: `xn--` is how a Unicode name is spelled in DNS, and a browser
/// shows such a host in Unicode, so an app named `xn--pple-43d` would show
/// as a host that looks like another name. None is issued as it stands.
fn idna_reserved(label: &str) -> bool {
    label.get(2..4) == Some("--")
}

/// Whether an app's name is a label it may have as it is.
fn own_label(app: &str) -> bool {
    valid_label(app) && !idna_reserved(app)
}

/// The label an app's name would have if nothing else held it, and whether
/// the name is a label already. A name that is (lower case, no `_`, short
/// enough) is its own label. Any other is lowered, `_` becomes `-`, and a
/// suffix from a hash of the exact name is added, so `Orders` and `orders`
/// and `or_ders` never meet.
fn derived(app: &str, attempt: u32) -> String {
    if attempt == 0 && own_label(app) {
        return app.to_string();
    }
    let lowered: String = app
        .chars()
        .map(|c| if c == '_' { '-' } else { c.to_ascii_lowercase() })
        .collect();
    let mut base: String = lowered.trim_matches('-').chars().take(MAX_LABEL - 9).collect();
    while base.ends_with('-') {
        base.pop();
    }
    let digest = Sha256::digest(format!("{app}\n{attempt}").as_bytes());
    let suffix: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    if base.is_empty() || idna_reserved(&base) {
        let base: String = format!("app-{base}").chars().take(MAX_LABEL - 9).collect();
        format!("{}-{suffix}", base.trim_end_matches('-'))
    } else {
        format!("{base}-{suffix}")
    }
}

// Every label ever issued, and the app it was issued to, is kept by the
// catalog (`.site/labels.json`, or `platform.host_labels`). A label is never
// issued to another app afterwards, even once its app is removed: a host's
// bookmarks, its storage in the browser and any cookie still held for it
// would otherwise pass to whoever published next under that name. An app
// published again under the same name, or put back from `.trash/`, gets
// its own label back.

/// Every top-level name that can be an app: a directory or a loose page.
fn app_names(config: &Config) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(&config.data_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let name = if entry.path().is_dir() { name } else { name.strip_suffix(".html")?.to_string() };
            valid_segment(&name).then_some(name)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

fn app_exists(config: &Config, app: &str) -> bool {
    config.data_dir.join(app).is_dir() || config.data_dir.join(format!("{app}.html")).is_file()
}

/// The app's label, assigned and stored the first time it is asked for. An
/// app that does not exist yet (an upload URL handed out before the upload)
/// gets the label it would be assigned, unstored.
///
/// Labels other apps hold or ever held are skipped (see the issued labels
/// above), and so is a name that is a label itself and has not been
/// assigned yet, so an app called `orders-1a2b3c4d` is not later moved off
/// its own name. The choice is made while the catalog holds the issued
/// labels, so two apps cannot both take a free one.
pub fn label_for(config: &Config, app: &str) -> String {
    if let Some(label) = catalog::meta_blocking(config, app).label.filter(|l| valid_label(l)) {
        return label;
    }
    // Labels other apps hold by their meta, or by their name. Read before
    // the hold: every label issued since is in the list the choice reads
    // inside it, since every assignment writes that list first.
    let mut held: std::collections::HashSet<String> = std::collections::HashSet::new();
    for other in app_names(config).into_iter().filter(|other| other != app) {
        match catalog::meta_blocking(config, &other).label {
            Some(label) => {
                held.insert(label);
            }
            None if own_label(&other) => {
                held.insert(other);
            }
            None => {}
        }
    }
    let exists = app_exists(config, app);
    let owner = app.to_string();
    let choose = Box::new(move |issued: &std::collections::BTreeMap<String, String>| {
        held.extend(issued.iter().filter(|(_, by)| **by != owner).map(|(label, _)| label.clone()));
        issued
            .iter()
            .find(|(label, by)| **by == owner && valid_label(label) && !held.contains(label.as_str()))
            .map(|(label, _)| label.clone())
            .unwrap_or_else(|| {
                (0..)
                    .map(|attempt| derived(&owner, attempt))
                    .find(|label| !held.contains(label))
                    .expect("an unbounded search finds a free label")
            })
    });
    let label = match catalog::of(config).assign_label_blocking(app, exists, choose) {
        Ok(label) => label,
        Err(why) => {
            // Nothing was issued, so nothing is stored: the label is derived
            // again, and issued, next time.
            tracing::warn!(app, %why, "a host label could not be issued");
            return derived(app, 0);
        }
    };
    if exists {
        let stored = label.clone();
        if let Err(why) = catalog::update_meta_blocking(config, app, move |meta| {
            meta.label = Some(stored);
            Ok(())
        }) {
            tracing::warn!(app, %why, "an app's host label could not be stored; it is derived again next time");
        }
    }
    label
}

/// The app a label was issued to, as the catalog has it. A list that
/// cannot be read names nobody, so the host is answered as unknown.
fn label_owner(config: &Config, label: &str) -> Option<String> {
    catalog::of(config).label_owner_blocking(label).unwrap_or_else(|why| {
        tracing::warn!(label, %why, "the issued host labels could not be read");
        None
    })
}

/// The app a label belongs to, if any.
pub fn app_for_label(config: &Config, label: &str) -> Option<String> {
    if !valid_label(label) {
        return None;
    }
    // Most labels are their app's own name, and every label issued is in
    // the list with its app.
    if app_exists(config, label) && label_for(config, label) == label {
        return Some(label.to_string());
    }
    if let Some(owner) = label_owner(config, label) {
        return (app_exists(config, &owner) && label_for(config, &owner) == label).then_some(owner);
    }
    app_names(config)
        .into_iter()
        .find(|app| app_exists(config, app) && label_for(config, app) == label)
}

/// The base an app's URLs are built on: its own origin in subdomain mode,
/// the site's address in path mode.
pub fn app_base(config: &Config, app: &str) -> String {
    match &config.apps {
        Some(apps) => apps.origin(&label_for(config, app)),
        None => config.base_url.clone().unwrap_or_else(|| config.local_base.clone()),
    }
}

/// The app's own origin, in subdomain mode only.
pub fn app_origin(config: &Config, app: &str) -> Option<String> {
    config.apps.as_ref().map(|apps| apps.origin(&label_for(config, app)))
}

/// The URL of a page or an app, as tools and messages print it: absolute
/// when the site knows its address, which subdomain mode always does.
pub fn page_url(config: &Config, slug: &str) -> String {
    let app = slug.split('/').next().unwrap_or(slug);
    match (app_origin(config, app), &config.base_url) {
        (Some(origin), _) => format!("{origin}/p/{slug}"),
        (None, Some(base)) => format!("{base}/p/{slug}"),
        (None, None) => format!("/p/{slug}"),
    }
}

/// A link to a page from one of toolsite's own pages: relative in path
/// mode, as those pages always linked, and on the app's host otherwise.
pub fn page_href(config: &Config, slug: &str) -> String {
    let app = slug.split('/').next().unwrap_or(slug);
    match app_origin(config, app) {
        Some(origin) => format!("{origin}/p/{slug}"),
        None => format!("/p/{slug}"),
    }
}

/// Set on a request that arrived on an app's host, naming the app, for the
/// few handlers outside `/p/` an app host also serves.
#[derive(Debug, Clone, PartialEq)]
pub struct AppHost(pub String);

/// Which host a request arrived on, in subdomain mode.
#[derive(Debug, Clone, PartialEq)]
pub enum Host {
    /// The site itself: toolsite's pages, MCP, uploads. No app content.
    Main,
    /// One app's host, by the app's name.
    App(String),
    /// Anything else, which is answered with 404.
    Unknown,
}

/// The authority of the site's base URL, lower case.
fn main_authority(config: &Config) -> Option<String> {
    let base = config.base_url.as_deref()?;
    let rest = base.split_once("://")?.1;
    Some(rest.split('/').next()?.to_ascii_lowercase())
}

/// Classifies a `Host` header. Only meaningful in subdomain mode; in path
/// mode every host is the main one.
///
/// The main host is the base URL's authority, or a loopback name, which
/// is what a renderer, a health check and the stdio transport's own client
/// reach the server by. An app host is one label in front of the apps
/// domain, on the expected port, whose label belongs to an app.
pub fn classify(config: &Config, host: Option<&str>) -> Host {
    let Some(apps) = &config.apps else {
        return Host::Main;
    };
    let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return Host::Unknown;
    };
    let host = host.to_ascii_lowercase();
    if main_authority(config).is_some_and(|main| main == host) {
        return Host::Main;
    }
    // `[::1]` or `[::1]:8080`, and nothing after the bracket but a port:
    // `[::1].apps.test` is not loopback.
    let name = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((name, tail)) if tail.is_empty() || tail.starts_with(':') => name,
            _ => "",
        },
        None => host.split(':').next().unwrap_or_default(),
    };
    if matches!(name, "localhost" | "127.0.0.1" | "::1") {
        return Host::Main;
    }
    match apps.label_of(&host) {
        Some(label) => match app_for_label(config, label) {
            Some(app) => Host::App(app),
            None => Host::Unknown,
        },
        None => Host::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(dir: &std::path::Path) -> Config {
        let mut config = Config::local(dir.to_path_buf(), "t");
        config.base_url = Some("https://site.test".into());
        config.apps = Some(AppsDomain::parse("apps.test", config.base_url.as_deref(), None).unwrap());
        config
    }

    fn app(config: &Config, name: &str) {
        std::fs::create_dir_all(config.data_dir.join(name)).unwrap();
        std::fs::write(config.data_dir.join(name).join("index.html"), "<p>hi</p>").unwrap();
    }

    #[test]
    fn a_name_that_is_a_label_is_its_own_and_any_other_gets_a_hashed_suffix() {
        assert_eq!(derived("orders", 0), "orders");
        let upper = derived("Orders", 0);
        assert!(upper.starts_with("orders-") && valid_label(&upper), "{upper}");
        assert_eq!(upper, derived("Orders", 0), "a label must be deterministic");
        assert_ne!(derived("or_ders", 0), derived("or-ders", 0));
        assert!(derived("_x_", 0).starts_with("x-"));
        let long = "a".repeat(80);
        assert!(derived(&long, 0).len() <= MAX_LABEL && valid_label(&derived(&long, 0)));
        assert!(valid_label(&derived("___", 0)));
        for spoof in ["xn--pple-43d", "ab--x", "XN--PPLE-43D"] {
            let label = derived(spoof, 0);
            assert!(valid_label(&label) && !idna_reserved(&label), "{spoof}: {label}");
        }
    }

    #[test]
    fn labels_stay_unique_under_case_and_underscore_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path());
        for name in ["orders", "Orders", "ORDERS", "or_ders", "or-ders"] {
            app(&config, name);
        }
        let labels: Vec<String> = ["orders", "Orders", "ORDERS", "or_ders", "or-ders"]
            .iter()
            .map(|name| label_for(&config, name))
            .collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "{labels:?}");
        assert_eq!(labels[0], "orders");
        assert_eq!(labels[4], "or-ders");
        for (name, label) in ["orders", "Orders", "ORDERS", "or_ders", "or-ders"].iter().zip(&labels) {
            assert_eq!(app_for_label(&config, label).as_deref(), Some(*name));
        }
    }

    #[test]
    fn a_label_once_stored_survives_a_name_that_would_now_claim_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path());
        app(&config, "Shop");
        let label = label_for(&config, "Shop");
        // An app later published under exactly that label does not take it.
        app(&config, &label);
        assert_eq!(label_for(&config, "Shop"), label);
        assert_ne!(label_for(&config, &label), label);
        assert_eq!(app_for_label(&config, &label).as_deref(), Some("Shop"));
    }

    #[test]
    fn a_forged_host_is_not_an_app_host() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(dir.path());
        app(&config, "orders");
        assert_eq!(classify(&config, Some("orders.apps.test")), Host::App("orders".into()));
        assert_eq!(classify(&config, Some("ORDERS.APPS.TEST")), Host::App("orders".into()));
        assert_eq!(classify(&config, Some("site.test")), Host::Main);
        assert_eq!(classify(&config, Some("127.0.0.1:8080")), Host::Main);
        assert_eq!(classify(&config, Some("[::1]:8080")), Host::Main);
        for forged in [
            "evil.com",
            "orders.apps.test.evil.com",
            "orders.apps.test.",
            "orders.apps.test:444",
            "x.orders.apps.test",
            "apps.test",
            "nothing.apps.test",
            "orders_apps.test",
            "site.test.evil.com",
            "[::1].apps.test",
            "[::1]x",
            "localhost.evil.com",
            "",
        ] {
            assert_eq!(classify(&config, Some(forged)), Host::Unknown, "{forged}");
        }
        assert_eq!(classify(&config, None), Host::Unknown);
    }

    #[test]
    fn the_apps_domain_takes_its_scheme_and_port_from_the_base_url() {
        let apps = AppsDomain::parse("apps.localhost", Some("http://localhost:18801"), None).unwrap();
        assert_eq!(apps.origin("orders"), "http://orders.apps.localhost:18801");
        assert!(apps.secure(), "browsers keep Secure cookies on localhost names");
        let apps = AppsDomain::parse("Apps.Example.com", Some("https://example.com"), None).unwrap();
        assert_eq!(apps.origin("orders"), "https://orders.apps.example.com");
        assert!(AppsDomain::parse("https://apps.example.com", Some("https://example.com"), None).is_err());
        assert!(AppsDomain::parse("apps.example.com", None, None).is_err());
        assert!(
            AppsDomain::parse("example.com", Some("https://admin.example.com"), None).is_err(),
            "a main site under the apps domain would share cookies with every app"
        );
    }
}
