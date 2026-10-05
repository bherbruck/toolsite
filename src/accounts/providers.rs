//! Signing in through someone else's login: Google, Microsoft, an Entra
//! tenant, GitHub, or anything that speaks OpenID Connect (Keycloak, Okta,
//! Auth0). The provider proves an email; this module decides which account
//! that is, and `users.rs` keeps the tables.
//!
//! Providers come from the environment, one group per slug:
//!
//! ```text
//! TOOLSITE_LOGIN_GOOGLE_CLIENT_ID=…          a preset: issuer known
//! TOOLSITE_LOGIN_GOOGLE_CLIENT_SECRET=…
//! TOOLSITE_LOGIN_ENTRA_TENANT=…              Entra wants its tenant
//! TOOLSITE_LOGIN_KEYCLOAK_ISSUER=https://…   anything else names its issuer
//! TOOLSITE_LOGIN_KEYCLOAK_NAME="Company SSO" optional button text
//! TOOLSITE_LOGIN_KEYCLOAK_ALLOW_DOMAIN=…     optional: create accounts for this domain
//! ```
//!
//! The policy is the same as everywhere else here: there is no public
//! signup. A provider login signs in the account that already holds that
//! email, linking the identity so a later email change does not lose it. Only
//! with `ALLOW_DOMAIN` does an unknown email become an account, and then only
//! for that domain, never as an admin. Everyone else is told to ask an admin.
//!
//! The protocol is the authorization-code flow with PKCE, a single-use state,
//! and a nonce checked against the id token, which is verified against the
//! provider's published keys. Nothing a browser can forge reaches the account
//! tables: the email comes from the provider's signed token or its userinfo
//! endpoint, never from the query string.

use crate::{
    accounts::users::{self, AtEmail},
    config::Config,
    content::slug::random_token,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use maud::html;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// A sign-in has this long between leaving for the provider and coming back.
const LOGIN_TTL: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const ENV_PREFIX: &str = "TOOLSITE_LOGIN_";

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// OpenID Connect. `issuer` is where discovery lives; `issuer_claim` is
    /// what the token's `iss` must be, which differs from the discovery URL
    /// only for Microsoft's multi-tenant endpoint.
    Oidc {
        issuer: String,
        issuer_claim: IssuerRule,
    },
    /// Plain OAuth 2: no id token, so the email comes from the API.
    GitHub,
}

/// What a token's `iss` may be.
#[derive(Debug, Clone, PartialEq)]
pub enum IssuerRule {
    Exactly(String),
    /// A template with one `{tenantid}` hole, which any single path segment
    /// fills: Microsoft's `common` endpoint issues tokens under the user's
    /// own tenant, and announces exactly this template in its discovery
    /// document.
    Template(String),
}

impl IssuerRule {
    pub fn matches(&self, actual: &str) -> bool {
        let trim = |s: &str| s.trim_end_matches('/').to_string();
        match self {
            IssuerRule::Exactly(expected) => trim(expected) == trim(actual),
            IssuerRule::Template(template) => {
                let Some((before, after)) = template.split_once("{tenantid}") else {
                    return false;
                };
                let actual = trim(actual);
                let after = after.trim_end_matches('/');
                let Some(rest) = actual.strip_prefix(before) else {
                    return false;
                };
                let Some(tenant) = rest.strip_suffix(after) else {
                    return false;
                };
                !tenant.is_empty() && !tenant.contains('/')
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    /// Lowercase; in the URLs and the identities table.
    pub slug: String,
    /// On the button.
    pub name: String,
    pub kind: Kind,
    pub client_id: String,
    pub client_secret: String,
    /// Emails under this domain get an account made for them on first
    /// sign-in. Everyone else needs one already.
    pub allow_domain: Option<String>,
}

impl Provider {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Oidc { .. } => "oidc",
            Kind::GitHub => "github",
        }
    }

    fn scopes(&self) -> &'static str {
        match self.kind {
            Kind::Oidc { .. } => "openid email profile",
            Kind::GitHub => "read:user user:email",
        }
    }
}

/// A sign-in that has left for the provider and not come back yet.
pub struct PendingLogin {
    pub provider: String,
    pub nonce: String,
    pub verifier: String,
    pub next: String,
    pub expires_at: Instant,
}

// --- configuration ---------------------------------------------------------

const FIELDS: [&str; 6] = [
    "CLIENT_SECRET",
    "CLIENT_ID",
    "ALLOW_DOMAIN",
    "ISSUER",
    "TENANT",
    "NAME",
];

/// Reads every `TOOLSITE_LOGIN_<SLUG>_<FIELD>` and builds the providers. A
/// group that is missing something is an error naming it, because a sign-in
/// button that fails on click is worse than a server that will not start.
pub fn from_env(vars: impl IntoIterator<Item = (String, String)>) -> Result<Vec<Provider>, String> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, BTreeMap<&'static str, String>> = BTreeMap::new();
    for (key, value) in vars {
        let Some(rest) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            continue;
        }
        let Some((slug, field)) = FIELDS.iter().find_map(|field| {
            rest.strip_suffix(field)
                .and_then(|head| head.strip_suffix('_'))
                .map(|slug| (slug, *field))
        }) else {
            return Err(format!(
                "{key}: not a provider setting; expected TOOLSITE_LOGIN_<SLUG>_<{}>",
                FIELDS.join("|")
            ));
        };
        if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
            return Err(format!("{key}: the provider slug must be letters and digits, like GOOGLE or KEYCLOAK"));
        }
        groups.entry(slug.to_string()).or_default().insert(field, value);
    }

    let mut providers = Vec::new();
    for (slug, fields) in groups {
        let need = |field: &str| -> Result<String, String> {
            fields
                .get(field)
                .cloned()
                .ok_or_else(|| format!("TOOLSITE_LOGIN_{slug}_{field} is missing"))
        };
        let client_id = need("CLIENT_ID")?;
        let client_secret = need("CLIENT_SECRET")?;
        let (kind, default_name) = match slug.as_str() {
            "GOOGLE" => (
                oidc("https://accounts.google.com"),
                "Google",
            ),
            "GITHUB" => (Kind::GitHub, "GitHub"),
            "MICROSOFT" => (
                Kind::Oidc {
                    issuer: "https://login.microsoftonline.com/common/v2.0".into(),
                    issuer_claim: IssuerRule::Template(
                        "https://login.microsoftonline.com/{tenantid}/v2.0".into(),
                    ),
                },
                "Microsoft",
            ),
            "ENTRA" => {
                let tenant = need("TENANT")?;
                (
                    oidc(&format!("https://login.microsoftonline.com/{tenant}/v2.0")),
                    "Microsoft Entra",
                )
            }
            _ => {
                let issuer = fields.get("ISSUER").cloned().ok_or_else(|| {
                    format!(
                        "TOOLSITE_LOGIN_{slug}_ISSUER is missing: {slug} is not a preset \
                         (GOOGLE, GITHUB, MICROSOFT, ENTRA), so say where its OpenID \
                         discovery lives"
                    )
                })?;
                if !issuer.starts_with("https://") && !issuer.starts_with("http://localhost") && !issuer.starts_with("http://127.0.0.1") {
                    return Err(format!("TOOLSITE_LOGIN_{slug}_ISSUER must be an https URL"));
                }
                (oidc(issuer.trim_end_matches('/')), "")
            }
        };
        let name = fields
            .get("NAME")
            .cloned()
            .unwrap_or_else(|| {
                if default_name.is_empty() {
                    let lower = slug.to_lowercase();
                    let mut chars = lower.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                        None => lower,
                    }
                } else {
                    default_name.to_string()
                }
            });
        let allow_domain = fields
            .get("ALLOW_DOMAIN")
            .map(|d| d.trim_start_matches('@').to_lowercase())
            .filter(|d| !d.is_empty());
        if let Some(domain) = &allow_domain
            && (domain.contains('@') || !domain.contains('.'))
        {
            return Err(format!("TOOLSITE_LOGIN_{slug}_ALLOW_DOMAIN should be a domain like example.com, not {domain:?}"));
        }
        providers.push(Provider {
            slug: slug.to_lowercase(),
            name,
            kind,
            client_id,
            client_secret,
            allow_domain,
        });
    }
    Ok(providers)
}

fn oidc(issuer: &str) -> Kind {
    Kind::Oidc {
        issuer: issuer.to_string(),
        issuer_claim: IssuerRule::Exactly(issuer.to_string()),
    }
}

/// Whether `email` is one the provider may create an account for.
pub fn domain_allowed(provider: &Provider, email: &str) -> bool {
    let Some(domain) = &provider.allow_domain else {
        return false;
    };
    email
        .rsplit_once('@')
        .is_some_and(|(_, at)| at.eq_ignore_ascii_case(domain))
}

fn find<'a>(config: &'a Config, slug: &str) -> Option<&'a Provider> {
    config.providers.iter().find(|p| p.slug == slug)
}

fn redirect_uri(config: &Config, slug: &str) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/auth/callback/{slug}")
}

// --- the provider's endpoints ---------------------------------------------

struct Endpoints {
    authorization: String,
    token: String,
    jwks: Option<String>,
    userinfo: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: Option<String>,
    userinfo_endpoint: Option<String>,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("toolsite/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client: {}", reason(&e)))
}

/// `Display` on a reqwest error drops the cause; walk to it.
fn reason(error: &reqwest::Error) -> String {
    let mut out = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

async fn endpoints(provider: &Provider) -> Result<Endpoints, String> {
    match &provider.kind {
        Kind::GitHub => Ok(Endpoints {
            authorization: "https://github.com/login/oauth/authorize".into(),
            token: "https://github.com/login/oauth/access_token".into(),
            jwks: None,
            userinfo: None,
        }),
        Kind::Oidc { issuer, .. } => {
            let url = format!("{issuer}/.well-known/openid-configuration");
            let response = client()?
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("discovery at {url}: {}", reason(&e)))?;
            if !response.status().is_success() {
                return Err(format!("discovery at {url} answered {}", response.status()));
            }
            let found: Discovery = response
                .json()
                .await
                .map_err(|e| format!("discovery at {url}: {}", reason(&e)))?;
            Ok(Endpoints {
                authorization: found.authorization_endpoint,
                token: found.token_endpoint,
                jwks: found.jwks_uri,
                userinfo: found.userinfo_endpoint,
            })
        }
    }
}

// --- leaving ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct Next {
    next: Option<String>,
}

/// `GET /auth/login/<slug>`: out to the provider, remembering where to come
/// back to.
pub async fn begin(
    State(config): State<Arc<Config>>,
    Path(slug): Path<String>,
    Query(params): Query<Next>,
) -> Response {
    let Some(provider) = find(&config, &slug) else {
        return (StatusCode::NOT_FOUND, "no such sign-in provider").into_response();
    };
    let endpoints = match endpoints(provider).await {
        Ok(endpoints) => endpoints,
        Err(why) => {
            tracing::warn!(provider = %slug, %why, "sign-in refused: provider not reachable");
            return refusal(
                StatusCode::BAD_GATEWAY,
                "That sign-in is not available right now",
                "The provider could not be reached. Try again in a moment, or sign in another way.",
            );
        }
    };

    let state = random_token(32);
    let nonce = random_token(32);
    let verifier = random_token(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let next = users::safe_next(params.next.as_deref());

    let Ok(mut url) = url::Url::parse(&endpoints.authorization) else {
        tracing::warn!(provider = %slug, "sign-in refused: provider's authorization endpoint is not a URL");
        return refusal(StatusCode::BAD_GATEWAY, "That sign-in is not available right now", "The provider's configuration is broken.");
    };
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &provider.client_id)
            .append_pair("redirect_uri", &redirect_uri(&config, &slug))
            .append_pair("scope", provider.scopes())
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        if matches!(provider.kind, Kind::Oidc { .. }) {
            query.append_pair("nonce", &nonce);
        }
    }

    {
        let now = Instant::now();
        let mut logins = config.logins.lock().unwrap();
        logins.retain(|_, pending| pending.expires_at > now);
        logins.insert(
            state,
            PendingLogin {
                provider: slug.clone(),
                nonce,
                verifier,
                next,
                expires_at: now + LOGIN_TTL,
            },
        );
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Redirect::to(url.as_str()),
    )
        .into_response()
}

// --- coming back -----------------------------------------------------------

#[derive(Deserialize)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// What the provider vouched for.
struct Proven {
    subject: String,
    email: String,
}

fn refusal(status: StatusCode, title: &str, detail: &str) -> Response {
    let markup = crate::ui::form_page(
        title,
        html! {
            h1 { (title) }
            p."muted" { (detail) }
            p { a."btn quiet" href="/auth/login" { "Back to sign in" } }
        },
    );
    (status, [(header::CACHE_CONTROL, "no-store")], Html(markup.into_string())).into_response()
}

/// `GET /auth/callback/<slug>`: the provider's answer.
pub async fn callback(
    State(config): State<Arc<Config>>,
    Path(slug): Path<String>,
    Query(params): Query<CallbackParams>,
) -> Response {
    let Some(provider) = find(&config, &slug) else {
        return (StatusCode::NOT_FOUND, "no such sign-in provider").into_response();
    };

    // The state is spent whatever happens next, so a replayed callback has
    // nothing to replay against.
    let pending = params.state.as_deref().and_then(|state| {
        let now = Instant::now();
        let mut logins = config.logins.lock().unwrap();
        logins.retain(|_, pending| pending.expires_at > now);
        logins.remove(state)
    });
    let Some(pending) = pending.filter(|pending| pending.provider == slug) else {
        tracing::warn!(provider = %slug, state_presented = params.state.is_some(), "sign-in refused: unknown, expired or spent state");
        return refusal(
            StatusCode::BAD_REQUEST,
            "That sign-in has expired",
            "Start again from the sign-in page.",
        );
    };

    if let Some(error) = params.error.as_deref() {
        tracing::warn!(provider = %slug, error, description = params.error_description.as_deref().unwrap_or(""), "sign-in refused by the provider");
        return refusal(
            StatusCode::BAD_REQUEST,
            "Sign-in did not complete",
            "The provider did not sign you in. You can try again.",
        );
    }
    let Some(code) = params.code.as_deref() else {
        tracing::warn!(provider = %slug, "sign-in refused: callback carried no code");
        return refusal(StatusCode::BAD_REQUEST, "Sign-in did not complete", "The provider sent nothing back.");
    };

    let proven = match prove(&config, provider, &pending, code).await {
        Ok(proven) => proven,
        Err(why) => {
            tracing::warn!(provider = %slug, %why, "sign-in refused: provider's answer did not check out");
            return refusal(
                StatusCode::BAD_GATEWAY,
                "Sign-in did not complete",
                "The provider's answer could not be verified. You can try again.",
            );
        }
    };

    // Which account this is, by the rules at the top of the file.
    let email = proven.email.to_lowercase();
    let domain_ok = domain_allowed(provider, &email);
    let (slug_owned, subject) = (slug.clone(), proven.subject.clone());
    let config_blocking = config.clone();
    let email_blocking = email.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<Result<String, Refused>, String> {
        let config = &config_blocking;
        if let Some(user) = users::user_by_identity(config, &slug_owned, &subject) {
            return Ok(users::start_session(config, &user.id).map_err(|_| Refused::Disabled));
        }
        let user = match users::account_at_email(config, &email_blocking)? {
            AtEmail::Active(user) => user,
            AtEmail::Disabled => return Ok(Err(Refused::Disabled)),
            AtEmail::Nobody if domain_ok => users::create_provider_account(config, &email_blocking)?,
            AtEmail::Nobody => return Ok(Err(Refused::NoAccount)),
        };
        users::link_identity(config, &slug_owned, &subject, &user.id)?;
        Ok(users::start_session(config, &user.id).map_err(|_| Refused::Disabled))
    })
    .await;

    match outcome {
        Ok(Ok(Ok(token))) => {
            tracing::info!(provider = %slug, %email, "signed in through a provider");
            (
                [
                    (header::SET_COOKIE, users::set_cookie_header(&token)),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                Redirect::to(&pending.next),
            )
                .into_response()
        }
        Ok(Ok(Err(Refused::Disabled))) => {
            tracing::warn!(provider = %slug, %email, "sign-in refused: account disabled");
            refusal(StatusCode::FORBIDDEN, "This account is disabled", "An admin turned it off. Ask them if that was a mistake.")
        }
        Ok(Ok(Err(Refused::NoAccount))) => {
            tracing::warn!(provider = %slug, %email, "sign-in refused: no account for that email");
            refusal(
                StatusCode::FORBIDDEN,
                "No account for that email",
                &format!("{email} signed in fine, but there is no account here for it. Ask an admin to add one, then try again."),
            )
        }
        Ok(Err(message)) => {
            tracing::warn!(provider = %slug, %message, "sign-in failed");
            refusal(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in did not complete", "Something went wrong on this side. Try again.")
        }
        Err(_) => refusal(StatusCode::INTERNAL_SERVER_ERROR, "Sign-in did not complete", "Something went wrong on this side. Try again."),
    }
}

enum Refused {
    Disabled,
    NoAccount,
}

/// Trades the code for what the provider knows about the person, and checks
/// every bit of it that can be checked. Only a verified email comes out.
async fn prove(config: &Config, provider: &Provider, pending: &PendingLogin, code: &str) -> Result<Proven, String> {
    let endpoints = endpoints(provider).await?;
    let client = client()?;
    // Built before the await, since the serializer itself is not Send.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", &redirect_uri(config, &provider.slug))
        .append_pair("client_id", &provider.client_id)
        .append_pair("client_secret", &provider.client_secret)
        .append_pair("code_verifier", &pending.verifier)
        .finish();
    let response = client
        .post(&endpoints.token)
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("token endpoint: {}", reason(&e)))?;
    let status = response.status();
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|e| format!("token endpoint answered {status} and then: {}", reason(&e)))?;
    if let Some(error) = tokens.error {
        return Err(format!(
            "token endpoint refused the code: {error} {}",
            tokens.error_description.unwrap_or_default()
        ));
    }

    match &provider.kind {
        Kind::GitHub => {
            let access = tokens.access_token.ok_or("token endpoint gave no access token")?;
            github_identity(&client, &access).await
        }
        Kind::Oidc { issuer_claim, .. } => {
            let id_token = tokens.id_token.ok_or("token endpoint gave no id_token")?;
            let jwks_uri = endpoints.jwks.ok_or("provider publishes no jwks_uri")?;
            let claims = verify_id_token(&client, &jwks_uri, &id_token, provider, issuer_claim, &pending.nonce).await?;
            let subject = claims
                .get("sub")
                .and_then(|v| v.as_str())
                .ok_or("id_token has no sub")?
                .to_string();
            let mut email = claims.get("email").and_then(|v| v.as_str()).map(str::to_string);
            let mut verified = claims.get("email_verified").and_then(|v| v.as_bool());
            if email.is_none() {
                let userinfo = endpoints.userinfo.ok_or("id_token has no email and the provider has no userinfo endpoint")?;
                let access = tokens.access_token.ok_or("id_token has no email and there is no access token for userinfo")?;
                let info: serde_json::Value = client
                    .get(&userinfo)
                    .bearer_auth(&access)
                    .send()
                    .await
                    .map_err(|e| format!("userinfo: {}", reason(&e)))?
                    .json()
                    .await
                    .map_err(|e| format!("userinfo: {}", reason(&e)))?;
                if info.get("sub").and_then(|v| v.as_str()) != Some(subject.as_str()) {
                    return Err("userinfo is about a different subject than the id_token".into());
                }
                email = info.get("email").and_then(|v| v.as_str()).map(str::to_string);
                verified = info.get("email_verified").and_then(|v| v.as_bool());
            }
            let email = email.ok_or("the provider did not say the person's email")?;
            // Google and Keycloak say; Entra does not carry the claim at all.
            // Refuse only an explicit no.
            if verified == Some(false) {
                return Err(format!("the provider says {email} is not verified"));
            }
            if !email.contains('@') {
                return Err(format!("{email:?} is not an email address"));
            }
            Ok(Proven { subject, email })
        }
    }
}

/// Signature against the provider's published key, then `aud`, `exp`, `iss`
/// and `nonce`. Only asymmetric algorithms: a provider's keys are public,
/// and a shared-secret token here would mean the secret had leaked.
async fn verify_id_token(
    client: &reqwest::Client,
    jwks_uri: &str,
    id_token: &str,
    provider: &Provider,
    issuer_claim: &IssuerRule,
    nonce: &str,
) -> Result<serde_json::Value, String> {
    use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};

    let header = decode_header(id_token).map_err(|e| format!("id_token header: {e}"))?;
    if !matches!(
        header.alg,
        Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 | Algorithm::ES256 | Algorithm::ES384 | Algorithm::PS256 | Algorithm::PS384 | Algorithm::PS512
    ) {
        return Err(format!("id_token uses {:?}, which is not an asymmetric algorithm", header.alg));
    }
    let keys: JwkSet = client
        .get(jwks_uri)
        .send()
        .await
        .map_err(|e| format!("jwks: {}", reason(&e)))?
        .json()
        .await
        .map_err(|e| format!("jwks: {}", reason(&e)))?;
    let key = match &header.kid {
        Some(kid) => keys.find(kid).ok_or_else(|| format!("no key {kid} in the provider's jwks"))?,
        None if keys.keys.len() == 1 => &keys.keys[0],
        None => return Err("id_token names no key and the provider publishes several".into()),
    };
    let decoding = DecodingKey::from_jwk(key).map_err(|e| format!("provider key: {e}"))?;

    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[&provider.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
    validation.leeway = 60;
    if let IssuerRule::Exactly(issuer) = issuer_claim {
        validation.set_issuer(&[issuer.as_str(), &format!("{issuer}/")]);
    }
    let data = decode::<serde_json::Value>(id_token, &decoding, &validation)
        .map_err(|e| format!("id_token did not verify: {e}"))?;
    let claims = data.claims;

    let iss = claims.get("iss").and_then(|v| v.as_str()).unwrap_or("");
    if !issuer_claim.matches(iss) {
        return Err(format!("id_token issuer {iss:?} is not this provider"));
    }
    match claims.get("nonce").and_then(|v| v.as_str()) {
        Some(got) if got == nonce => {}
        Some(_) => return Err("id_token nonce does not match this sign-in".into()),
        None => return Err("id_token carries no nonce".into()),
    }
    Ok(claims)
}

/// GitHub: who the token belongs to, and their primary verified email.
async fn github_identity(client: &reqwest::Client, access: &str) -> Result<Proven, String> {
    #[derive(Deserialize)]
    struct GhUser {
        id: u64,
    }
    #[derive(Deserialize)]
    struct GhEmail {
        email: String,
        primary: bool,
        verified: bool,
    }
    let user: GhUser = client
        .get("https://api.github.com/user")
        .bearer_auth(access)
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("github user: {}", reason(&e)))?
        .json()
        .await
        .map_err(|e| format!("github user: {}", reason(&e)))?;
    let emails: Vec<GhEmail> = client
        .get("https://api.github.com/user/emails")
        .bearer_auth(access)
        .header(header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("github emails: {}", reason(&e)))?
        .json()
        .await
        .map_err(|e| format!("github emails: {}", reason(&e)))?;
    let email = emails
        .iter()
        .find(|e| e.primary && e.verified)
        .or_else(|| emails.iter().find(|e| e.verified))
        .map(|e| e.email.clone())
        .ok_or("the GitHub account has no verified email")?;
    Ok(Proven {
        subject: user.id.to_string(),
        email,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_preset_needs_only_its_client_and_knows_its_issuer() {
        let providers = from_env(env(&[
            ("TOOLSITE_LOGIN_GOOGLE_CLIENT_ID", "g-id"),
            ("TOOLSITE_LOGIN_GOOGLE_CLIENT_SECRET", "g-secret"),
            ("TOOLSITE_LOGIN_GITHUB_CLIENT_ID", "gh-id"),
            ("TOOLSITE_LOGIN_GITHUB_CLIENT_SECRET", "gh-secret"),
            ("UNRELATED", "x"),
        ]))
        .unwrap();
        assert_eq!(providers.len(), 2);
        let github = providers.iter().find(|p| p.slug == "github").unwrap();
        assert_eq!((github.name.as_str(), github.kind.clone()), ("GitHub", Kind::GitHub));
        let google = providers.iter().find(|p| p.slug == "google").unwrap();
        assert_eq!(google.name, "Google");
        assert_eq!(google.kind, oidc("https://accounts.google.com"));
        assert!(google.allow_domain.is_none());
    }

    #[test]
    fn entra_takes_a_tenant_and_microsoft_takes_any() {
        let providers = from_env(env(&[
            ("TOOLSITE_LOGIN_ENTRA_TENANT", "contoso.onmicrosoft.com"),
            ("TOOLSITE_LOGIN_ENTRA_CLIENT_ID", "e-id"),
            ("TOOLSITE_LOGIN_ENTRA_CLIENT_SECRET", "e-secret"),
            ("TOOLSITE_LOGIN_MICROSOFT_CLIENT_ID", "m-id"),
            ("TOOLSITE_LOGIN_MICROSOFT_CLIENT_SECRET", "m-secret"),
        ]))
        .unwrap();
        let entra = providers.iter().find(|p| p.slug == "entra").unwrap();
        assert_eq!(entra.kind, oidc("https://login.microsoftonline.com/contoso.onmicrosoft.com/v2.0"));
        let microsoft = providers.iter().find(|p| p.slug == "microsoft").unwrap();
        let Kind::Oidc { issuer_claim, .. } = &microsoft.kind else { panic!() };
        assert!(issuer_claim.matches("https://login.microsoftonline.com/9188040d-6c67-4c5b-b112-36a304b66dad/v2.0"));
        assert!(!issuer_claim.matches("https://evil.example/9188040d/v2.0"));
        assert!(!issuer_claim.matches("https://login.microsoftonline.com/a/b/v2.0"));
        assert!(!issuer_claim.matches("https://login.microsoftonline.com//v2.0"));

        let missing = from_env(env(&[
            ("TOOLSITE_LOGIN_ENTRA_CLIENT_ID", "e-id"),
            ("TOOLSITE_LOGIN_ENTRA_CLIENT_SECRET", "e-secret"),
        ]))
        .unwrap_err();
        assert!(missing.contains("ENTRA_TENANT"), "{missing}");
    }

    #[test]
    fn anything_else_must_name_its_issuer() {
        let providers = from_env(env(&[
            ("TOOLSITE_LOGIN_KEYCLOAK_ISSUER", "https://sso.example.com/realms/main/"),
            ("TOOLSITE_LOGIN_KEYCLOAK_CLIENT_ID", "k-id"),
            ("TOOLSITE_LOGIN_KEYCLOAK_CLIENT_SECRET", "k-secret"),
            ("TOOLSITE_LOGIN_KEYCLOAK_NAME", "Company SSO"),
            ("TOOLSITE_LOGIN_KEYCLOAK_ALLOW_DOMAIN", "@Example.com"),
        ]))
        .unwrap();
        let keycloak = &providers[0];
        assert_eq!(keycloak.slug, "keycloak");
        assert_eq!(keycloak.name, "Company SSO");
        assert_eq!(keycloak.kind, oidc("https://sso.example.com/realms/main"));
        assert_eq!(keycloak.allow_domain.as_deref(), Some("example.com"));

        let error = from_env(env(&[
            ("TOOLSITE_LOGIN_OKTA_CLIENT_ID", "o-id"),
            ("TOOLSITE_LOGIN_OKTA_CLIENT_SECRET", "o-secret"),
        ]))
        .unwrap_err();
        assert!(error.contains("OKTA_ISSUER"), "{error}");

        let unnamed = from_env(env(&[
            ("TOOLSITE_LOGIN_OKTA_ISSUER", "https://x.okta.com"),
            ("TOOLSITE_LOGIN_OKTA_CLIENT_ID", "o-id"),
            ("TOOLSITE_LOGIN_OKTA_CLIENT_SECRET", "o-secret"),
        ]))
        .unwrap();
        assert_eq!(unnamed[0].name, "Okta");
    }

    #[test]
    fn a_group_missing_its_secret_or_misspelt_is_a_startup_error() {
        let error = from_env(env(&[("TOOLSITE_LOGIN_GOOGLE_CLIENT_ID", "g-id")])).unwrap_err();
        assert!(error.contains("GOOGLE_CLIENT_SECRET"), "{error}");
        let error = from_env(env(&[("TOOLSITE_LOGIN_GOOGLE_CLIENTID", "g-id")])).unwrap_err();
        assert!(error.contains("not a provider setting"), "{error}");
        let error = from_env(env(&[
            ("TOOLSITE_LOGIN_MY_IDP_ISSUER", "https://x"),
            ("TOOLSITE_LOGIN_MY_IDP_CLIENT_ID", "x"),
            ("TOOLSITE_LOGIN_MY_IDP_CLIENT_SECRET", "x"),
        ]))
        .unwrap_err();
        assert!(error.contains("slug"), "{error}");
        // Empty values are the same as unset, which is how a dashboard leaves them.
        assert!(from_env(env(&[("TOOLSITE_LOGIN_GOOGLE_CLIENT_ID", "  ")])).unwrap().is_empty());
    }

    #[test]
    fn an_allowed_domain_matches_the_whole_domain_and_nothing_near_it() {
        let provider = Provider {
            slug: "x".into(),
            name: "X".into(),
            kind: Kind::GitHub,
            client_id: "".into(),
            client_secret: "".into(),
            allow_domain: Some("example.com".into()),
        };
        assert!(domain_allowed(&provider, "a@example.com"));
        assert!(domain_allowed(&provider, "a@EXAMPLE.com"));
        assert!(!domain_allowed(&provider, "a@notexample.com"));
        assert!(!domain_allowed(&provider, "a@example.com.evil"));
        assert!(!domain_allowed(&provider, "a@sub.example.com"));
        assert!(!domain_allowed(&provider, "example.com"));
        let none = Provider { allow_domain: None, ..provider };
        assert!(!domain_allowed(&none, "a@example.com"));
    }

    #[test]
    fn an_exact_issuer_tolerates_only_a_trailing_slash() {
        let rule = IssuerRule::Exactly("https://accounts.google.com".into());
        assert!(rule.matches("https://accounts.google.com"));
        assert!(rule.matches("https://accounts.google.com/"));
        assert!(!rule.matches("https://accounts.google.com.evil"));
        assert!(!rule.matches("http://accounts.google.com"));
    }
}
