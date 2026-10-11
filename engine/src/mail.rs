//! Mail: reading (the submodules) and outgoing mail — each IMAP account's
//! SMTP settings, drafts, and sending.
//!
//! A draft is prefilled from the message it answers or forwards, edited by
//! the user (or written by the chat agent's `draft_email` tool), and sent only
//! by [`send_draft`], which the apps call from the user's send button. The
//! agent has no way to send.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Local, TimeZone, Utc};
use lettre::message::header::{ContentTransferEncoding, ContentType};
use lettre::message::{Attachment, Body, Mailbox, Mailboxes, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use mnemis_types::{DraftDto, DraftKind, DraftStatus, SmtpSettingsDto};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::secrets;
use crate::source::SourceId;

pub mod cache;
pub mod ops;
pub mod sync;
pub mod thread;

/// How long composing waits for the original's headers before going on
/// without them.
const ORIGINAL_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const SMTP_TIMEOUT: Duration = Duration::from_secs(60);

/// What `source/{id}/smtp` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SmtpAccount {
    server: String,
    port: u16,
    #[serde(default)]
    starttls: bool,
    username: String,
    /// Keychain entry of a password of its own; `None` reuses the IMAP one.
    #[serde(default)]
    password_ref: Option<String>,
    from_name: String,
    from_address: String,
    #[serde(default)]
    save_sent: bool,
}

fn smtp_key(source_id: i64) -> String {
    format!("source/{source_id}/smtp")
}

/// An account's SMTP settings: the saved ones, or guesses from its IMAP
/// settings (`imap.host` → `smtp.host`, port 465, the same login).
pub async fn smtp_settings(pool: &SqlitePool, source_id: i64) -> Result<SmtpSettingsDto> {
    let (account, saved) = load_account(pool, source_id).await?;
    Ok(SmtpSettingsDto {
        source_id,
        server: account.server,
        port: account.port,
        starttls: account.starttls,
        username: account.username,
        own_password: account.password_ref.is_some(),
        from_name: account.from_name,
        from_address: account.from_address,
        save_sent: account.save_sent,
        saved,
    })
}

async fn load_account(pool: &SqlitePool, source_id: i64) -> Result<(SmtpAccount, bool)> {
    if let Some(json) = crate::settings::get_setting(pool, &smtp_key(source_id)).await? {
        let account = serde_json::from_str(&json).context("parsing SMTP settings")?;
        return Ok((account, true));
    }
    let imap = crate::settings::get_setting(pool, &format!("source/{source_id}/imap"))
        .await?
        .context("not a mail account")?;
    let imap: serde_json::Value = serde_json::from_str(&imap)?;
    let imap_server = imap["server"].as_str().unwrap_or_default();
    let username = imap["username"].as_str().unwrap_or_default().to_string();
    let server = match imap_server.strip_prefix("imap.") {
        Some(rest) => format!("smtp.{rest}"),
        None => imap_server.to_string(),
    };
    let profile = crate::settings::get_user_profile(pool).await?;
    let from_address = if username.contains('@') {
        username.clone()
    } else {
        String::new()
    };
    Ok((
        SmtpAccount {
            server,
            port: 465,
            starttls: false,
            username,
            password_ref: None,
            from_name: profile.display_name,
            from_address,
            save_sent: false,
        },
        false,
    ))
}

/// Save an account's SMTP settings. A non-empty `password` is stored as the
/// account's own; without one, `own_password` keeps the stored one or, when
/// false, goes back to reusing the IMAP password.
pub async fn save_smtp_settings(
    pool: &SqlitePool,
    settings: &SmtpSettingsDto,
    password: Option<&str>,
) -> Result<()> {
    if settings.server.trim().is_empty() {
        bail!("the SMTP server is required");
    }
    if settings.from_address.parse::<lettre::Address>().is_err() {
        bail!("\"{}\" is not an email address", settings.from_address);
    }
    let (current, _) = load_account(pool, settings.source_id).await?;
    let password_ref = match password.filter(|p| !p.is_empty()) {
        Some(password) => {
            let reference = format!(
                "smtp/{}@{}",
                settings.username.trim(),
                settings.server.trim()
            );
            secrets::store(&reference, password)
                .await
                .context("storing the SMTP password in the keychain")?;
            Some(reference)
        }
        None if settings.own_password => current.password_ref,
        None => None,
    };
    let account = SmtpAccount {
        server: settings.server.trim().to_string(),
        port: settings.port,
        starttls: settings.starttls,
        username: settings.username.trim().to_string(),
        password_ref,
        from_name: settings.from_name.trim().to_string(),
        from_address: settings.from_address.trim().to_string(),
        save_sent: settings.save_sent,
    };
    crate::settings::set_setting(
        pool,
        &smtp_key(settings.source_id),
        &serde_json::to_string(&account)?,
    )
    .await
}

/// What the network side of sending does; [`Network`] in the apps, a
/// recorder in tests.
#[async_trait]
pub trait Postbox: Send + Sync {
    /// Relay a finished message over SMTP.
    async fn send(
        &self,
        source_id: i64,
        envelope: &lettre::address::Envelope,
        raw: &[u8],
    ) -> Result<()>;
    /// The raw bytes of a received message.
    async fn fetch_original(
        &self,
        source_id: i64,
        mailbox: &str,
        external_id: &str,
    ) -> Result<Vec<u8>>;
    /// File a copy of sent mail in the account's sent folder.
    async fn file_sent(&self, source_id: i64, raw: &[u8]) -> Result<()>;
}

/// The real network: SMTP for sending, the account's IMAP for the rest.
pub struct Network<'a> {
    pub pool: &'a SqlitePool,
}

#[async_trait]
impl Postbox for Network<'_> {
    async fn send(
        &self,
        source_id: i64,
        envelope: &lettre::address::Envelope,
        raw: &[u8],
    ) -> Result<()> {
        let (account, _) = load_account(self.pool, source_id).await?;
        let password_ref = match &account.password_ref {
            Some(reference) => reference.clone(),
            None => {
                let (config_ref,): (String,) =
                    sqlx::query_as("SELECT config_ref FROM sources WHERE id = ?")
                        .bind(source_id)
                        .fetch_one(self.pool)
                        .await?;
                config_ref
            }
        };
        let password = secrets::fetch(&password_ref)
            .await
            .with_context(|| format!("fetching keychain entry {password_ref}"))?;
        let builder = if account.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.server)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&account.server)?
        };
        let transport = builder
            .port(account.port)
            .credentials(Credentials::new(account.username.clone(), password))
            .timeout(Some(SMTP_TIMEOUT))
            .build();
        transport
            .send_raw(envelope, raw)
            .await
            .with_context(|| format!("sending through {}", account.server))?;
        Ok(())
    }

    async fn fetch_original(
        &self,
        source_id: i64,
        mailbox: &str,
        external_id: &str,
    ) -> Result<Vec<u8>> {
        let source = crate::orchestrator::build_imap_source(self.pool, SourceId(source_id)).await?;
        source.fetch_raw(mailbox, external_id).await
    }

    async fn file_sent(&self, source_id: i64, raw: &[u8]) -> Result<()> {
        let source = crate::orchestrator::build_imap_source(self.pool, SourceId(source_id)).await?;
        source.append_to_sent(raw).await
    }
}

/// The message a draft answers, as stored.
struct Original {
    source_id: i64,
    mailbox: String,
    external_id: String,
    subject: String,
    body: String,
    posted_at: i64,
    author_name: Option<String>,
    author_address: Option<String>,
    recipients: Vec<crate::source::Recipient>,
}

async fn load_original(pool: &SqlitePool, message_id: i64) -> Result<Original> {
    #[allow(clippy::type_complexity)]
    let row: (
        i64,
        String,
        String,
        Option<String>,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT c.source_id, c.external_id, m.external_id, m.subject, m.body, m.posted_at, \
                p.display_name, p.external_id, m.recipients_json \
         FROM messages m JOIN channels c ON c.id = m.channel_id \
         LEFT JOIN people p ON p.id = m.author_id WHERE m.id = ?",
    )
    .bind(message_id)
    .fetch_one(pool)
    .await
    .context("loading the original message")?;
    Ok(Original {
        source_id: row.0,
        mailbox: row.1,
        external_id: row.2,
        subject: row.3.unwrap_or_default(),
        body: row.4,
        posted_at: row.5,
        author_name: row.6,
        author_address: row.7.filter(|a| a.contains('@')),
        recipients: row
            .8
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
    })
}

/// Headers of the original that mnemis doesn't store.
#[derive(Default, Clone)]
struct OriginalHeaders {
    reply_to: Vec<(Option<String>, String)>,
    references: Option<String>,
}

fn parse_headers(raw: &[u8]) -> OriginalHeaders {
    let Some(msg) = mail_parser::MessageParser::default().parse(raw) else {
        return OriginalHeaders::default();
    };
    let reply_to = msg
        .reply_to()
        .map(|a| {
            a.iter()
                .filter_map(|a| {
                    let address = a.address.as_deref()?.to_string();
                    Some((a.name.as_deref().map(str::to_string), address))
                })
                .collect()
        })
        .unwrap_or_default();
    let references = match msg.references() {
        mail_parser::HeaderValue::Text(id) => Some(format!("<{id}>")),
        mail_parser::HeaderValue::TextList(ids) => Some(
            ids.iter()
                .map(|id| format!("<{id}>"))
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    OriginalHeaders {
        reply_to,
        references,
    }
}

/// Parsed headers of originals, by database and message. The message itself
/// lands in the mail cache; this shares a fetch in flight, so a reply
/// started during [`prefetch_original`] waits for it instead of fetching
/// again.
type HeaderCell = Arc<tokio::sync::OnceCell<OriginalHeaders>>;
static HEADERS: LazyLock<Mutex<HashMap<(PathBuf, i64), HeaderCell>>> =
    LazyLock::new(Default::default);
/// Entries kept before the cache starts over; one per opened email.
const HEADER_CACHE_SIZE: usize = 64;

fn header_cell(pool: &SqlitePool, message_id: i64) -> HeaderCell {
    let key = (
        pool.connect_options().get_filename().to_path_buf(),
        message_id,
    );
    let mut cache = HEADERS.lock().expect("header cache lock");
    if cache.len() >= HEADER_CACHE_SIZE && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.entry(key).or_default().clone()
}

/// The original's Reply-To and References, from the cache or the server;
/// empty when the server doesn't answer in time.
async fn original_headers(
    pool: &SqlitePool,
    postbox: &dyn Postbox,
    message_id: i64,
    original: &Original,
) -> OriginalHeaders {
    let cell = header_cell(pool, message_id);
    let fetch = cell.get_or_try_init(|| async {
        let raw = original_raw(pool, postbox, original).await?;
        anyhow::Ok(parse_headers(&raw))
    });
    match tokio::time::timeout(ORIGINAL_FETCH_TIMEOUT, fetch).await {
        Ok(Ok(headers)) => headers.clone(),
        _ => OriginalHeaders::default(),
    }
}

/// The original's full message: from the mail cache, else from the server
/// (and then cached).
async fn original_raw(
    pool: &SqlitePool,
    postbox: &dyn Postbox,
    original: &Original,
) -> Result<Vec<u8>> {
    let mut conn = pool.acquire().await?;
    if let Some(raw) = cache::get(&mut conn, original.source_id, &original.external_id).await? {
        return Ok(raw);
    }
    drop(conn);
    let raw = postbox
        .fetch_original(original.source_id, &original.mailbox, &original.external_id)
        .await?;
    let mut conn = pool.acquire().await?;
    cache::store(
        &mut conn,
        original.source_id,
        &original.external_id,
        &raw,
        Utc::now().timestamp(),
    )
    .await?;
    Ok(raw)
}

/// Fetch what replying to a message needs ahead of time, for when the user
/// opens it: the reply then starts without a round trip to the server.
pub async fn prefetch_original(
    pool: &SqlitePool,
    postbox: &dyn Postbox,
    message_id: i64,
) -> Result<()> {
    let original = load_original(pool, message_id).await?;
    original_headers(pool, postbox, message_id, &original).await;
    Ok(())
}

/// Message-IDs are stored without their angle brackets; ids mnemis made up
/// for mail without one aren't Message-IDs at all.
fn message_id_header(external_id: &str) -> Option<String> {
    (!external_id.starts_with("imap-uid-") && external_id.contains('@'))
        .then(|| format!("<{}>", external_id.trim_matches(['<', '>'])))
}

fn format_mailbox(name: Option<&str>, address: &str) -> String {
    let name = name.filter(|n| !n.trim().is_empty()).map(str::to_string);
    match (name, address.parse()) {
        (Some(name), Ok(parsed)) => Mailbox::new(Some(name), parsed).to_string(),
        (Some(name), Err(_)) => format!("{name} <{address}>"),
        (None, _) => address.to_string(),
    }
}

fn prefixed(prefix: &str, subject: &str) -> String {
    let lower = subject.trim_start().to_lowercase();
    let already = match prefix {
        "Re:" => lower.starts_with("re:"),
        _ => lower.starts_with("fwd:") || lower.starts_with("fw:"),
    };
    if already {
        subject.trim().to_string()
    } else {
        format!("{prefix} {}", subject.trim())
    }
}

fn local_date(at: i64) -> String {
    Local
        .timestamp_opt(at, 0)
        .single()
        .map(|d| d.format("%a, %-d %b %Y at %H:%M").to_string())
        .unwrap_or_default()
}

fn quoted(original: &Original) -> String {
    let who = original
        .author_name
        .as_deref()
        .or(original.author_address.as_deref())
        .unwrap_or("someone");
    let mut out = format!("On {}, {who} wrote:\n", local_date(original.posted_at));
    for line in original.body.trim_end().lines() {
        if line.is_empty() {
            out.push_str(">\n");
        } else {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn forwarded(original: &Original) -> String {
    let from = match (&original.author_name, &original.author_address) {
        (Some(name), Some(address)) => format!("{name} <{address}>"),
        (Some(name), None) => name.clone(),
        (None, Some(address)) => address.clone(),
        (None, None) => String::new(),
    };
    let to: Vec<String> = original
        .recipients
        .iter()
        .filter(|r| r.kind == "to")
        .filter_map(|r| r.address.clone())
        .collect();
    format!(
        "---------- Forwarded message ----------\nFrom: {from}\nDate: {}\nSubject: {}\nTo: {}\n\n{}\n",
        local_date(original.posted_at),
        original.subject,
        to.join(", "),
        original.body.trim_end()
    )
}

/// The user's own addresses, never to be replied to: their profile's and
/// every account's.
async fn own_addresses(pool: &SqlitePool) -> Result<Vec<String>> {
    let mut out: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT ci.value FROM contact_identifiers ci JOIN contacts c ON c.id = ci.contact_id \
         WHERE c.relationship = 'self' AND ci.kind = 'email'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(v,)| v)
    .collect();
    let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM sources WHERE kind = 'imap'")
        .fetch_all(pool)
        .await?;
    for (id,) in ids {
        if let Ok((account, _)) = load_account(pool, id).await {
            out.push(account.from_address);
            out.push(account.username);
        }
    }
    Ok(out
        .into_iter()
        .filter(|a| a.contains('@'))
        .map(|a| a.to_lowercase())
        .collect())
}

/// The addresses the user can send as: their profile's, then each account's
/// sender address, without repeats.
async fn sender_addresses(pool: &SqlitePool) -> Result<Vec<String>> {
    let mut candidates: Vec<String> = sqlx::query_as::<_, (String,)>(
        "SELECT ci.value FROM contact_identifiers ci JOIN contacts c ON c.id = ci.contact_id \
         WHERE c.relationship = 'self' AND ci.kind = 'email' ORDER BY ci.id",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(v,)| v.trim().to_string())
    .collect();
    for id in mail_accounts(pool).await? {
        if let Ok((account, _)) = load_account(pool, id).await {
            candidates.push(account.from_address);
        }
    }
    let mut out: Vec<String> = Vec::new();
    for address in candidates {
        if address.parse::<lettre::Address>().is_ok()
            && !out.iter().any(|a| a.eq_ignore_ascii_case(&address))
        {
            out.push(address);
        }
    }
    Ok(out)
}

async fn mail_accounts(pool: &SqlitePool) -> Result<Vec<i64>> {
    let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM sources WHERE kind = 'imap' ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(ids.into_iter().map(|(id,)| id).collect())
}

/// The account that sends as `address`: the one whose sender or login it
/// is, else `fallback`, else the first.
async fn account_for(
    pool: &SqlitePool,
    address: &str,
    fallback: Option<i64>,
) -> Result<Option<i64>> {
    let accounts = mail_accounts(pool).await?;
    for &id in &accounts {
        if let Ok((account, _)) = load_account(pool, id).await
            && (account.from_address.eq_ignore_ascii_case(address)
                || account.username.eq_ignore_ascii_case(address))
        {
            return Ok(Some(id));
        }
    }
    Ok(fallback.or(accounts.first().copied()))
}

/// The name mail goes out under: the account's, else the profile's.
async fn sender_name(pool: &SqlitePool, source_id: Option<i64>) -> Result<String> {
    if let Some(id) = source_id
        && let Ok((account, _)) = load_account(pool, id).await
        && !account.from_name.trim().is_empty()
    {
        return Ok(account.from_name);
    }
    Ok(crate::settings::get_user_profile(pool).await?.display_name)
}

/// The account's own sender address, when it has one.
async fn account_address(pool: &SqlitePool, source_id: Option<i64>) -> Option<String> {
    let (account, _) = load_account(pool, source_id?).await.ok()?;
    Some(account.from_address).filter(|a| !a.is_empty())
}

/// A new draft, prefilled for its kind: a reply goes to the original's
/// Reply-To or sender (a reply-all also to its other recipients, minus the
/// user) with the original quoted; a forward carries the original's text.
/// New mail goes from the first mail account.
pub async fn start_draft(
    pool: &SqlitePool,
    postbox: &dyn Postbox,
    kind: DraftKind,
    message_id: Option<i64>,
    chat_id: Option<i64>,
    by_agent: bool,
) -> Result<i64> {
    let mut to = String::new();
    let mut cc = String::new();
    let mut subject = String::new();
    let mut body = String::new();
    let mut in_reply_to = None;
    let mut references = None;
    // Addresses the original went to (or came from, for mail the user sent),
    // to answer from the same one.
    let mut addressed: Vec<String> = Vec::new();
    let source_id = match (kind, message_id) {
        (DraftKind::New, _) => {
            let first: Option<(i64,)> =
                sqlx::query_as("SELECT id FROM sources WHERE kind = 'imap' ORDER BY id LIMIT 1")
                    .fetch_optional(pool)
                    .await?;
            first.map(|(id,)| id)
        }
        (_, None) => bail!("a reply or forward needs the message it answers"),
        (kind, Some(message_id)) => {
            let original = load_original(pool, message_id).await?;
            addressed.extend(original.author_address.clone());
            for kind in ["to", "cc"] {
                addressed.extend(
                    original
                        .recipients
                        .iter()
                        .filter(|r| r.kind == kind)
                        .filter_map(|r| r.address.clone()),
                );
            }
            if kind == DraftKind::Forward {
                subject = prefixed("Fwd:", &original.subject);
                body = format!("\n\n{}", forwarded(&original));
            } else {
                let headers = original_headers(pool, postbox, message_id, &original).await;
                let own = own_addresses(pool).await?;
                let is_own = |a: &str| own.contains(&a.to_lowercase());
                let mut to_list: Vec<(Option<String>, String)> = headers.reply_to;
                if to_list.is_empty()
                    && let Some(address) = &original.author_address
                {
                    to_list.push((original.author_name.clone(), address.clone()));
                }
                // Replying to mail the user sent goes to its recipients.
                if to_list.iter().all(|(_, a)| is_own(a)) {
                    to_list = original
                        .recipients
                        .iter()
                        .filter(|r| r.kind == "to")
                        .filter_map(|r| Some((r.name.clone(), r.address.clone()?)))
                        .collect();
                }
                let mut cc_list = Vec::new();
                if kind == DraftKind::ReplyAll {
                    for r in &original.recipients {
                        let Some(address) = &r.address else { continue };
                        let taken = to_list.iter().chain(&cc_list).any(
                            |(_, a): &(Option<String>, String)| a.eq_ignore_ascii_case(address),
                        );
                        if !taken && !is_own(address) {
                            cc_list.push((r.name.clone(), address.clone()));
                        }
                    }
                }
                let join = |list: &[(Option<String>, String)]| {
                    list.iter()
                        .map(|(name, address)| format_mailbox(name.as_deref(), address))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                to = join(&to_list);
                cc = join(&cc_list);
                subject = prefixed("Re:", &original.subject);
                body = format!("\n\n{}", quoted(&original));
                in_reply_to = message_id_header(&original.external_id);
                references = match (headers.references, &in_reply_to) {
                    (Some(refs), Some(id)) => Some(format!("{refs} {id}")),
                    (None, id) => id.clone(),
                    (refs, None) => refs,
                };
            }
            Some(original.source_id)
        }
    };
    let senders = sender_addresses(pool).await?;
    let from_addr = addressed
        .iter()
        .find_map(|a| senders.iter().find(|s| s.eq_ignore_ascii_case(a)))
        .cloned()
        .or(account_address(pool, source_id).await)
        .or_else(|| senders.first().cloned());
    let source_id = match &from_addr {
        Some(address) => account_for(pool, address, source_id).await?,
        None => source_id,
    };
    let now = Utc::now().timestamp();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO drafts (source_id, kind, message_id, chat_id, to_addrs, cc_addrs, subject, \
                             body, in_reply_to, \"references\", created_by, created_at, updated_at, \
                             from_addr) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(source_id)
    .bind(kind.as_str())
    .bind(message_id)
    .bind(chat_id)
    .bind(&to)
    .bind(&cc)
    .bind(&subject)
    .bind(&body)
    .bind(&in_reply_to)
    .bind(&references)
    .bind(if by_agent { "agent" } else { "user" })
    .bind(now)
    .bind(now)
    .bind(&from_addr)
    .fetch_one(pool)
    .await
    .context("inserting draft")?;
    Ok(id)
}

#[derive(sqlx::FromRow)]
struct DraftRow {
    id: i64,
    source_id: Option<i64>,
    kind: String,
    message_id: Option<i64>,
    chat_id: Option<i64>,
    to_addrs: String,
    cc_addrs: String,
    bcc_addrs: String,
    subject: String,
    body: String,
    in_reply_to: Option<String>,
    references: Option<String>,
    status: String,
    error: Option<String>,
    created_by: String,
    updated_at: i64,
    sent_at: Option<i64>,
    from_addr: Option<String>,
}

async fn load_draft(pool: &SqlitePool, id: i64) -> Result<DraftRow> {
    sqlx::query_as(
        "SELECT id, source_id, kind, message_id, chat_id, to_addrs, cc_addrs, bcc_addrs, \
                subject, body, in_reply_to, \"references\", status, error, created_by, \
                updated_at, sent_at, from_addr \
         FROM drafts WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("no draft {id}"))
}

pub async fn get_draft(pool: &SqlitePool, id: i64) -> Result<DraftDto> {
    let row = load_draft(pool, id).await?;
    let from_address = match row.from_addr.clone() {
        Some(address) => address,
        None => account_address(pool, row.source_id)
            .await
            .unwrap_or_default(),
    };
    let mut from_choices = sender_addresses(pool).await?;
    if !from_address.is_empty()
        && !from_choices
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&from_address))
    {
        from_choices.insert(0, from_address.clone());
    }
    Ok(DraftDto {
        id: row.id,
        source_id: row.source_id,
        kind: DraftKind::parse(&row.kind).unwrap_or(DraftKind::New),
        message_id: row.message_id,
        chat_id: row.chat_id,
        from_name: sender_name(pool, row.source_id).await?,
        from_address,
        from_choices,
        to: row.to_addrs,
        cc: row.cc_addrs,
        bcc: row.bcc_addrs,
        subject: row.subject,
        body: row.body,
        status: DraftStatus::parse(&row.status).unwrap_or(DraftStatus::Draft),
        error: row.error,
        by_agent: row.created_by == "agent",
        updated_at: row.updated_at,
        sent_at: row.sent_at,
    })
}

/// Store the user's edits: the sender address, addressees, subject and body.
/// The address picks the account that sends it. A sent or discarded draft is
/// left as it is.
pub async fn save_draft(pool: &SqlitePool, draft: &DraftDto) -> Result<()> {
    let from_addr = Some(draft.from_address.trim().to_string()).filter(|a| !a.is_empty());
    let source_id = match &from_addr {
        Some(address) => account_for(pool, address, draft.source_id).await?,
        None => draft.source_id,
    };
    sqlx::query(
        "UPDATE drafts SET source_id = ?, from_addr = ?, to_addrs = ?, cc_addrs = ?, \
                           bcc_addrs = ?, subject = ?, body = ?, updated_at = ? \
         WHERE id = ? AND status = 'draft'",
    )
    .bind(source_id)
    .bind(&from_addr)
    .bind(&draft.to)
    .bind(&draft.cc)
    .bind(&draft.bcc)
    .bind(&draft.subject)
    .bind(&draft.body)
    .bind(Utc::now().timestamp())
    .bind(draft.id)
    .execute(pool)
    .await
    .context("saving draft")?;
    Ok(())
}

pub async fn discard_draft(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE drafts SET status = 'discarded', updated_at = ? WHERE id = ? AND status = 'draft'",
    )
    .bind(Utc::now().timestamp())
    .bind(id)
    .execute(pool)
    .await
    .context("discarding draft")?;
    Ok(())
}

fn parse_list(field: &str, list: &str) -> Result<Vec<Mailbox>> {
    if list.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Mailboxes = list
        .trim()
        .trim_end_matches(',')
        .parse()
        .map_err(|e| anyhow::anyhow!("{field}: {e}"))?;
    Ok(parsed.into_iter().collect())
}

/// The finished message for a draft. `original` is the forwarded mail,
/// attached whole.
fn build_message(
    row: &DraftRow,
    from: Mailbox,
    message_id: &str,
    original: Option<&[u8]>,
) -> Result<Message> {
    let to = parse_list("To", &row.to_addrs)?;
    let cc = parse_list("Cc", &row.cc_addrs)?;
    let bcc = parse_list("Bcc", &row.bcc_addrs)?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        bail!("add at least one recipient");
    }
    let mut builder = Message::builder()
        .from(from)
        .subject(row.subject.trim())
        .message_id(Some(message_id.to_string()));
    for m in to {
        builder = builder.to(m);
    }
    for m in cc {
        builder = builder.cc(m);
    }
    for m in bcc {
        builder = builder.bcc(m);
    }
    if let Some(id) = &row.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(refs) = &row.references {
        builder = builder.references(refs.clone());
    }
    let text = SinglePart::plain(row.body.clone());
    let message = match original {
        None => builder.singlepart(text)?,
        Some(raw) => {
            // RFC 2046 keeps message/rfc822 parts unencoded where it can.
            let body = Body::new_with_encoding(raw.to_vec(), ContentTransferEncoding::EightBit)
                .or_else(|raw| Body::new_with_encoding(raw, ContentTransferEncoding::Base64))
                .map_err(|_| anyhow::anyhow!("can't encode the forwarded message"))?;
            let name = format!(
                "{}.eml",
                row.subject
                    .trim()
                    .trim_start_matches("Fwd:")
                    .trim()
                    .replace(['/', '\\', ':'], "-")
            );
            let attachment = Attachment::new(name).body(
                body,
                ContentType::parse("message/rfc822").expect("static content type"),
            );
            builder.multipart(MultiPart::mixed().singlepart(text).singlepart(attachment))?
        }
    };
    Ok(message)
}

/// Send a draft: build it, relay it, file the copy when the account asks,
/// and mark it sent. On failure the draft keeps its status and records the
/// error. A failure only to file the copy still marks it sent, with a note.
pub async fn send_draft(pool: &SqlitePool, postbox: &dyn Postbox, id: i64) -> Result<DraftDto> {
    let result = send_inner(pool, postbox, id).await;
    if let Err(e) = &result {
        sqlx::query(
            "UPDATE drafts SET error = ?, updated_at = ? WHERE id = ? AND status = 'draft'",
        )
        .bind(format!("{e:#}"))
        .bind(Utc::now().timestamp())
        .bind(id)
        .execute(pool)
        .await?;
    }
    result?;
    get_draft(pool, id).await
}

async fn send_inner(pool: &SqlitePool, postbox: &dyn Postbox, id: i64) -> Result<()> {
    let row = load_draft(pool, id).await?;
    if row.status != "draft" {
        bail!("this draft was already {}", row.status);
    }
    let source_id = row.source_id.context("add a mail account to send from")?;
    let (account, _) = load_account(pool, source_id).await?;
    let address = match row.from_addr.clone() {
        Some(address) => address,
        None => account.from_address.clone(),
    };
    let parsed: lettre::Address = address
        .parse()
        .map_err(|_| anyhow::anyhow!("choose a sender address"))?;
    let name = sender_name(pool, Some(source_id)).await?;
    let from = Mailbox::new(Some(name).filter(|n| !n.trim().is_empty()), parsed);
    let domain = address
        .rsplit('@')
        .next()
        .unwrap_or("mnemis.invalid")
        .to_string();
    let message_id = format!("<{}@{domain}>", uuid::Uuid::new_v4());
    let original = match (row.kind.as_str(), row.message_id) {
        ("forward", Some(message_id)) => {
            let o = load_original(pool, message_id).await?;
            Some(
                original_raw(pool, postbox, &o)
                    .await
                    .context("fetching the message to forward")?,
            )
        }
        _ => None,
    };
    let message = build_message(&row, from, &message_id, original.as_deref())?;
    let raw = message.formatted();
    postbox.send(source_id, message.envelope(), &raw).await?;

    let filing = if account.save_sent {
        postbox
            .file_sent(source_id, &raw)
            .await
            .err()
            .map(|e| format!("Sent, but the copy wasn't filed in Sent: {e:#}"))
    } else {
        None
    };
    let now = Utc::now().timestamp();
    sqlx::query(
        "UPDATE drafts SET status = 'sent', sent_at = ?, updated_at = ?, error = ?, \
                           sent_message_id = ? WHERE id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(filing)
    .bind(message_id.trim_matches(['<', '>']))
    .bind(id)
    .execute(pool)
    .await
    .context("marking draft sent")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[derive(Default)]
    struct Recorder {
        original: Option<Vec<u8>>,
        sent: Mutex<Vec<(Vec<String>, String)>>,
        filed: Mutex<usize>,
    }

    #[async_trait]
    impl Postbox for Recorder {
        async fn send(
            &self,
            _source_id: i64,
            envelope: &lettre::address::Envelope,
            raw: &[u8],
        ) -> Result<()> {
            let to = envelope.to().iter().map(|a| a.to_string()).collect();
            self.sent
                .lock()
                .unwrap()
                .push((to, String::from_utf8_lossy(raw).into_owned()));
            Ok(())
        }
        async fn fetch_original(&self, _: i64, _: &str, _: &str) -> Result<Vec<u8>> {
            self.original.clone().context("offline")
        }
        async fn file_sent(&self, _: i64, _: &[u8]) -> Result<()> {
            *self.filed.lock().unwrap() += 1;
            Ok(())
        }
    }

    const ORIGINAL: &str = "From: Ana Souza <ana@example.com>\r\n\
        Reply-To: Projeto <projeto@example.com>\r\n\
        To: Gustavo <me@example.org>, bob@example.net\r\n\
        Cc: carol@example.net\r\n\
        Message-ID: <abc@example.com>\r\n\
        References: <root@example.com>\r\n\
        Subject: =?utf-8?q?Revis=C3=A3o?= do contrato\r\n\
        \r\n\
        Pode revisar ate sexta?\r\n";

    /// A mail account (source 1, me@example.org) holding ORIGINAL as
    /// message 1.
    async fn setup() -> (TempDir, SqlitePool) {
        let tmp = TempDir::new().unwrap();
        let pool = crate::db::open(&tmp.path().join("t.db")).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO sources (id, kind, name, config_ref, created_at) \
             VALUES (1, 'imap', 'Mail', 'imap/me@example.org@imap.example.org', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::settings::set_setting(
            &pool,
            "source/1/imap",
            r#"{"server":"imap.example.org","port":993,"username":"me@example.org"}"#,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO channels (id, source_id, external_id, name, kind) \
             VALUES (1, 1, 'INBOX', 'INBOX', 'mailbox')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO people (id, source_id, external_id, display_name) \
             VALUES (1, 1, 'ana@example.com', 'Ana Souza')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, channel_id, external_id, author_id, posted_at, subject, \
                                   body, body_format, ingested_at, recipients_json) \
             VALUES (1, 1, 'abc@example.com', 1, 0, 'Revisão do contrato', \
                     'Pode revisar até sexta?', 'text', 0, ?)",
        )
        .bind(
            r#"[{"kind":"to","name":"Gustavo","address":"me@example.org"},
                {"kind":"to","address":"bob@example.net"},
                {"kind":"cc","address":"carol@example.net"}]"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        (tmp, pool)
    }

    #[tokio::test]
    async fn guesses_smtp_settings_from_the_imap_account() {
        let (_tmp, pool) = setup().await;
        let s = smtp_settings(&pool, 1).await.unwrap();
        assert_eq!(s.server, "smtp.example.org");
        assert_eq!(s.port, 465);
        assert_eq!(s.from_address, "me@example.org");
        assert!(!s.saved && !s.own_password && !s.save_sent);
    }

    #[tokio::test]
    async fn reply_all_answers_the_reply_to_and_copies_the_others_but_not_the_user() {
        let (_tmp, pool) = setup().await;
        let postbox = Recorder {
            original: Some(ORIGINAL.as_bytes().to_vec()),
            ..Default::default()
        };
        let id = start_draft(&pool, &postbox, DraftKind::ReplyAll, Some(1), None, false)
            .await
            .unwrap();
        let d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.to, "Projeto <projeto@example.com>");
        assert_eq!(d.cc, "bob@example.net, carol@example.net");
        assert_eq!(d.subject, "Re: Revisão do contrato");
        assert!(d.body.contains("> Pode revisar até sexta?"), "{}", d.body);
        assert_eq!(d.from_address, "me@example.org");

        let mut edited = d.clone();
        edited.body = format!("Sim, reviso.{}", d.body);
        save_draft(&pool, &edited).await.unwrap();
        let sent = send_draft(&pool, &postbox, id).await.unwrap();
        assert_eq!(sent.status, DraftStatus::Sent);
        let (rcpt, raw) = postbox.sent.lock().unwrap()[0].clone();
        assert_eq!(rcpt.len(), 3);
        assert!(raw.contains("In-Reply-To: <abc@example.com>"), "{raw}");
        assert!(
            raw.contains("References: <root@example.com> <abc@example.com>"),
            "{raw}"
        );
        assert_eq!(*postbox.filed.lock().unwrap(), 0);

        // A sent draft can't go out twice.
        assert!(send_draft(&pool, &postbox, id).await.is_err());
    }

    #[tokio::test]
    async fn reply_falls_back_to_the_sender_when_the_server_is_unreachable() {
        let (_tmp, pool) = setup().await;
        let id = start_draft(
            &pool,
            &Recorder::default(),
            DraftKind::Reply,
            Some(1),
            None,
            true,
        )
        .await
        .unwrap();
        let d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.to, "Ana Souza <ana@example.com>");
        assert!(d.cc.is_empty());
        assert!(d.by_agent);
    }

    #[tokio::test]
    async fn forward_attaches_the_original_and_files_a_copy_when_asked() {
        let (_tmp, pool) = setup().await;
        let mut settings = smtp_settings(&pool, 1).await.unwrap();
        settings.save_sent = true;
        settings.from_name = "Gustavo".into();
        save_smtp_settings(&pool, &settings, None).await.unwrap();
        let postbox = Recorder {
            original: Some(ORIGINAL.as_bytes().to_vec()),
            ..Default::default()
        };
        let id = start_draft(&pool, &postbox, DraftKind::Forward, Some(1), None, false)
            .await
            .unwrap();
        let mut d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.subject, "Fwd: Revisão do contrato");
        assert!(d.body.contains("Forwarded message"));
        assert!(d.to.is_empty());

        // Nobody to send to yet: the error is kept on the draft.
        assert!(send_draft(&pool, &postbox, id).await.is_err());
        assert!(get_draft(&pool, id).await.unwrap().error.is_some());

        d.to = "dave@example.net".into();
        save_draft(&pool, &d).await.unwrap();
        let sent = send_draft(&pool, &postbox, id).await.unwrap();
        assert_eq!(sent.status, DraftStatus::Sent);
        assert!(sent.error.is_none());
        let (_, raw) = postbox.sent.lock().unwrap()[0].clone();
        assert!(raw.contains("message/rfc822"), "{raw}");
        assert!(raw.contains("Message-ID: <abc@example.com>"), "{raw}");
        assert!(raw.contains("From: Gustavo <me@example.org>"), "{raw}");
        assert_eq!(*postbox.filed.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_prefetched_original_serves_the_reply_without_the_server() {
        let (_tmp, pool) = setup().await;
        let online = Recorder {
            original: Some(ORIGINAL.as_bytes().to_vec()),
            ..Default::default()
        };
        prefetch_original(&pool, &online, 1).await.unwrap();
        // Offline now, yet the reply still goes to the Reply-To.
        let id = start_draft(
            &pool,
            &Recorder::default(),
            DraftKind::Reply,
            Some(1),
            None,
            false,
        )
        .await
        .unwrap();
        let d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.to, "Projeto <projeto@example.com>");
    }

    #[tokio::test]
    async fn answers_from_the_address_the_original_was_sent_to() {
        let (_tmp, pool) = setup().await;
        sqlx::query(
            "INSERT INTO contacts (id, display_name, relationship, created_at, updated_at) \
             VALUES (1, 'Me', 'self', 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for address in ["other@example.org", "Alias@example.org"] {
            sqlx::query(
                "INSERT INTO contact_identifiers (contact_id, kind, value) VALUES (1, 'email', ?)",
            )
            .bind(address)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE messages SET recipients_json = ? WHERE id = 1")
            .bind(r#"[{"kind":"to","address":"alias@example.org"}]"#)
            .execute(&pool)
            .await
            .unwrap();

        let postbox = Recorder::default();
        let id = start_draft(&pool, &postbox, DraftKind::Reply, Some(1), None, false)
            .await
            .unwrap();
        let d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.from_address, "Alias@example.org");
        assert_eq!(
            d.from_choices,
            ["other@example.org", "Alias@example.org", "me@example.org"]
        );
        assert_eq!(d.source_id, Some(1));

        let sent = send_draft(&pool, &postbox, id).await.unwrap();
        assert_eq!(sent.status, DraftStatus::Sent);
        let (_, raw) = postbox.sent.lock().unwrap()[0].clone();
        assert!(raw.contains("From: Alias@example.org"), "{raw}");

        // A new mail starts from the account's own address.
        let id = start_draft(&pool, &postbox, DraftKind::New, None, None, false)
            .await
            .unwrap();
        assert_eq!(
            get_draft(&pool, id).await.unwrap().from_address,
            "me@example.org"
        );
    }

    #[tokio::test]
    async fn discarded_drafts_are_not_sent() {
        let (_tmp, pool) = setup().await;
        let postbox = Recorder::default();
        let id = start_draft(&pool, &postbox, DraftKind::New, None, None, false)
            .await
            .unwrap();
        let mut d = get_draft(&pool, id).await.unwrap();
        assert_eq!(d.source_id, Some(1));
        d.to = "x@example.net".into();
        save_draft(&pool, &d).await.unwrap();
        discard_draft(&pool, id).await.unwrap();
        assert!(send_draft(&pool, &postbox, id).await.is_err());
        assert!(postbox.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn prefixes_subjects_once() {
        assert_eq!(prefixed("Re:", "RE: x"), "RE: x");
        assert_eq!(prefixed("Re:", "x"), "Re: x");
        assert_eq!(prefixed("Fwd:", "Fw: x"), "Fw: x");
        assert_eq!(message_id_header("imap-uid-1-2"), None);
        assert_eq!(message_id_header("a@b"), Some("<a@b>".into()));
    }
}
