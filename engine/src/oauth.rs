//! Signing in to Google, the way RFC 8252 asks of native apps: the
//! authorization-code flow with PKCE in the system browser (never a web view
//! of our own), back to a loopback address on the desktop or the iOS client's
//! own URL scheme. What it yields is a refresh token, kept in the keychain;
//! access tokens are minted from it on use and live only in memory.
//!
//! The OAuth clients belong to whoever builds mnemis (a Google Cloud
//! project): `engine/build.rs` bakes their ids in from `git config`
//! (`mnemis.googleDesktopClientId`, `mnemis.googleDesktopClientSecret`,
//! `mnemis.googleIosClientId`) or the matching `MNEMIS_GOOGLE_*` variables.
//! A build without them can't add Google accounts and says so.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
/// IMAP and SMTP take only the full mail scope; `email` names the account.
const SCOPES: &str = "https://mail.google.com/ openid email";
/// Gmail's servers.
pub const IMAP_SERVER: &str = "imap.gmail.com";
pub const IMAP_PORT: u16 = 993;
/// How long the desktop waits for the browser to come back.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// An access token this close to expiring is renewed first.
const SLACK: Duration = Duration::from_secs(60);

/// This build's OAuth client: the desktop one, or on iOS the iOS one.
struct Client {
    id: &'static str,
    /// Desktop clients get a "secret" Google wants on token requests; an
    /// installed app can't keep it, so it isn't one.
    secret: Option<&'static str>,
}

fn client() -> Result<Client> {
    #[cfg(target_os = "ios")]
    let client = option_env!("MNEMIS_GOOGLE_IOS_CLIENT_ID").map(|id| Client { id, secret: None });
    #[cfg(not(target_os = "ios"))]
    let client = option_env!("MNEMIS_GOOGLE_DESKTOP_CLIENT_ID").map(|id| Client {
        id,
        secret: option_env!("MNEMIS_GOOGLE_DESKTOP_CLIENT_SECRET"),
    });
    client
        .filter(|c| !c.id.is_empty())
        .context("Google sign-in isn't configured in this build")
}

/// Whether this build can add Google accounts.
pub fn available() -> bool {
    client().is_ok()
}

/// Where Google comes back to in the iOS app.
pub fn app_redirect() -> Result<String> {
    Ok(ios_redirect(client()?.id))
}

/// The iOS client's redirect: its id reversed, as a URL scheme.
pub fn ios_redirect(client_id: &str) -> String {
    let reversed: Vec<&str> = client_id.split('.').rev().collect();
    format!("{}:/oauth2redirect", reversed.join("."))
}

/// A sign-in under way: what the browser was sent with, to check and finish
/// what comes back.
#[derive(Debug, Clone)]
pub struct Pending {
    verifier: String,
    state: String,
    redirect_uri: String,
}

/// What a finished sign-in yields.
#[derive(Debug, Clone)]
pub struct SignedIn {
    pub email: String,
    pub refresh_token: String,
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// RFC 7636's S256 challenge for `verifier`.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Where to send the browser, and what to check its return against.
pub fn begin(redirect_uri: &str) -> Result<(String, Pending)> {
    let client = client()?;
    let pending = Pending {
        verifier: random_token()?,
        state: random_token()?,
        redirect_uri: redirect_uri.to_string(),
    };
    Ok((auth_url(client.id, &pending), pending))
}

fn auth_url(client_id: &str, pending: &Pending) -> String {
    let mut url = reqwest::Url::parse(AUTH_URL).expect("a valid URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &pending.redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge(&pending.verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pending.state)
        // A refresh token, and a fresh one even when this account signed in
        // before (Google issues it only with consent).
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    url.into()
}

/// The authorization code in what the browser came back with (a URL, or
/// just its `?query`), once its `state` is ours.
fn code_from(pending: &Pending, callback: &str) -> Result<String> {
    let query = callback.split_once('?').map_or(callback, |(_, q)| q);
    let query = query.split('#').next().unwrap_or_default();
    let pairs: HashMap<String, String> = url_pairs(query);
    if pairs.get("state") != Some(&pending.state) {
        bail!("the sign-in came back for another request; try again");
    }
    if let Some(error) = pairs.get("error") {
        if error == "access_denied" {
            bail!("Google sign-in was cancelled");
        }
        bail!("Google sign-in failed: {error}");
    }
    pairs
        .get("code")
        .cloned()
        .context("Google sent no authorization code")
}

fn url_pairs(query: &str) -> HashMap<String, String> {
    let mut url = reqwest::Url::parse("http://x/").expect("a valid URL");
    url.set_query(Some(query));
    url.query_pairs().into_owned().collect()
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

async fn token_request(form: &[(&str, &str)]) -> Result<TokenReply> {
    let client = client()?;
    let mut form = form.to_vec();
    form.push(("client_id", client.id));
    if let Some(secret) = client.secret {
        form.push(("client_secret", secret));
    }
    let response = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("reaching Google")?;
    if response.status().is_success() {
        return response.json().await.context("reading Google's token");
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    match serde_json::from_str::<TokenError>(&body) {
        // Revoked, expired, or the password changed.
        Ok(e) if e.error == "invalid_grant" => {
            bail!("Google no longer accepts this account's sign-in; remove it and add it again")
        }
        Ok(e) => bail!(
            "Google refused the token request: {}{}",
            e.error,
            e.error_description
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ),
        Err(_) => bail!("Google refused the token request: {status}"),
    }
}

/// Trade what the browser came back with for the account's tokens.
pub async fn finish(pending: &Pending, callback: &str) -> Result<SignedIn> {
    let code = code_from(pending, callback)?;
    let reply = token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("code_verifier", &pending.verifier),
        ("redirect_uri", &pending.redirect_uri),
    ])
    .await?;
    let refresh_token = reply
        .refresh_token
        .context("Google issued no refresh token")?;
    #[derive(Deserialize)]
    struct UserInfo {
        email: String,
    }
    let info: UserInfo = reqwest::Client::new()
        .get(USERINFO_URL)
        .bearer_auth(&reply.access_token)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("asking Google for the account's address")?
        .error_for_status()
        .context("asking Google for the account's address")?
        .json()
        .await?;
    Ok(SignedIn {
        email: info.email,
        refresh_token,
    })
}

/// The desktop sign-in: open the browser at Google (through `open`), wait
/// for it to come back to a loopback address, and finish.
pub async fn sign_in_desktop(open: impl FnOnce(&str) -> Result<()>) -> Result<SignedIn> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("listening for the browser")?;
    let port = listener.local_addr()?.port();
    let (url, pending) = begin(&format!("http://127.0.0.1:{port}"))?;
    open(&url)?;
    let wait = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]);
            // GET /?state=…&code=… HTTP/1.1
            let target = request.split_whitespace().nth(1).unwrap_or_default();
            if !target.contains("state=") {
                // The browser asking for a favicon, say.
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }
            let result = code_from(&pending, target).map(|_| target.to_string());
            let page = match &result {
                Ok(_) => "Signed in. You can close this tab and go back to mnemis.".to_string(),
                Err(e) => format!("{e:#}"),
            };
            let page = format!(
                "<!doctype html><meta charset=utf-8><title>mnemis</title>\
                 <body style=\"font:16px system-ui;margin:3em\">{page}</body>"
            );
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
                        page.len()
                    )
                    .as_bytes(),
                )
                .await;
            return result;
        }
    };
    let callback = tokio::time::timeout(SIGN_IN_TIMEOUT, wait)
        .await
        .context("Google sign-in timed out")??;
    finish(&pending, &callback).await
}

/// Access tokens minted this run, by the keychain entry of their refresh
/// token.
static ACCESS: LazyLock<Mutex<HashMap<String, (String, Instant)>>> =
    LazyLock::new(Default::default);

/// A current access token for the account whose refresh token the keychain
/// keeps as `refresh_ref`.
pub async fn access_token(refresh_ref: &str) -> Result<String> {
    if let Some((token, until)) = ACCESS.lock().expect("token cache").get(refresh_ref)
        && *until > Instant::now() + SLACK
    {
        return Ok(token.clone());
    }
    let refresh_token = crate::secrets::fetch(refresh_ref)
        .await
        .with_context(|| format!("fetching keychain entry {refresh_ref}"))?;
    let reply = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh_token),
    ])
    .await?;
    let until = Instant::now() + Duration::from_secs(reply.expires_in);
    ACCESS
        .lock()
        .expect("token cache")
        .insert(refresh_ref.to_string(), (reply.access_token.clone(), until));
    Ok(reply.access_token)
}

/// Drop a cached access token the server turned down, so the next
/// [`access_token`] mints a new one.
pub fn forget(refresh_ref: &str) {
    ACCESS.lock().expect("token cache").remove(refresh_ref);
}

/// SASL XOAUTH2's initial response (before base64).
pub fn xoauth2(email: &str, access_token: &str) -> String {
    format!("user={email}\x01auth=Bearer {access_token}\x01\x01")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> Pending {
        Pending {
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            state: "s1".into(),
            redirect_uri: "http://127.0.0.1:4711".into(),
        }
    }

    #[test]
    fn challenge_matches_rfc_7636() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn asks_for_a_refresh_token_with_pkce() {
        let url = auth_url("id.apps.googleusercontent.com", &pending());
        let pairs = url_pairs(url.split_once('?').unwrap().1);
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], challenge(&pending().verifier));
        assert_eq!(pairs["access_type"], "offline");
        assert_eq!(pairs["prompt"], "consent");
        assert_eq!(pairs["state"], "s1");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:4711");
        assert!(pairs["scope"].contains("https://mail.google.com/"));
    }

    #[test]
    fn takes_the_code_only_from_its_own_request() {
        let p = pending();
        assert_eq!(code_from(&p, "/?state=s1&code=4%2Fabc").unwrap(), "4/abc");
        assert_eq!(
            code_from(
                &p,
                "com.googleusercontent.apps.1:/oauth2redirect?code=c&state=s1"
            )
            .unwrap(),
            "c"
        );
        assert!(code_from(&p, "/?state=other&code=c").is_err());
        let cancelled = code_from(&p, "/?state=s1&error=access_denied").unwrap_err();
        assert!(format!("{cancelled}").contains("cancelled"));
    }

    #[test]
    fn redirects_to_the_reversed_ios_client_id() {
        assert_eq!(
            ios_redirect("123-abc.apps.googleusercontent.com"),
            "com.googleusercontent.apps.123-abc:/oauth2redirect"
        );
    }
}
