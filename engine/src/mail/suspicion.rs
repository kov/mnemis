//! Signs that a message isn't from who it claims to be, for the mail list's
//! warnings and for extraction: a forged date ([`super::misdated`]), a sender
//! its domain didn't authorize (DMARC, as the receiving server judged it),
//! links whose text shows one site and lead to another, and a first email
//! from a domain under a name that has nothing to do with it. The last two
//! are weak alone: tracked newsletters and new correspondents trip them.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::source::links::{Link, registrable};

/// "New to the user" means something only against enough history: until the
/// database holds this many earlier messages, spanning `HISTORY_DAYS`, no
/// sender is judged (on a fresh install, every one is new).
const HISTORY_MESSAGES: i64 = 200;
const HISTORY_DAYS: i64 = 30;

/// Misleading links named in a reason.
const NAMED_LINKS: usize = 2;

/// What the receiving server concluded about the sender, from its
/// `Authentication-Results` headers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Auth {
    /// The DMARC result ("pass", "fail", "none", …), when it was checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmarc: Option<String>,
    /// The From domain DMARC was checked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The server put a DMARC failure down to a mailing list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mailing_list: bool,
    /// The ARC result: relays that pass mail on sign it ("none" when none
    /// did).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc: Option<String>,
}

impl Auth {
    /// The results in a message's header section: every
    /// `Authentication-Results` header from the server that added the
    /// topmost one (Fastmail splits its results over several), and none
    /// from elsewhere, which a sender can write.
    pub fn from_headers(raw: &[u8]) -> Auth {
        let values: Vec<String> = header_values(raw, "authentication-results");
        let Some(server) = values.first().map(|v| authserv_id(v).to_string()) else {
            return Auth::default();
        };
        let mut auth = Auth::default();
        for value in values.iter().filter(|v| authserv_id(v) == server) {
            for item in value.split(';').skip(1).map(str::trim) {
                if let Some(rest) = item.strip_prefix("dmarc=") {
                    auth.dmarc = Some(word(rest));
                    auth.from = tokens(rest)
                        .find_map(|t| t.strip_prefix("header.from="))
                        .map(str::to_lowercase);
                    auth.mailing_list =
                        tokens(rest).any(|t| t == "policy.override-reason=mailing_list");
                } else if let Some(rest) = item.strip_prefix("arc=") {
                    auth.arc = Some(word(rest));
                }
            }
        }
        auth
    }

    /// Why the sender looks unauthorized: DMARC failed, and neither a
    /// mailing list nor a relay (which signs ARC) explains it, as both
    /// break DMARC for genuine mail.
    pub fn failure(&self) -> Option<String> {
        let relayed = self.mailing_list || self.arc.as_deref().is_some_and(|a| a != "none");
        (self.dmarc.as_deref() == Some("fail") && !relayed).then(|| match &self.from {
            Some(domain) => format!("{domain} didn't authorize its sender (DMARC failed)"),
            None => "its sender failed authentication (DMARC)".to_string(),
        })
    }
}

/// The unfolded values of every `name` header in a message's header
/// section, top first.
fn header_values(raw: &[u8], name: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let head = text
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .split("\n\n")
        .next()
        .unwrap_or_default();
    let mut headers: Vec<String> = Vec::new();
    for line in head.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else {
            headers.push(line.trim_end().to_string());
        }
    }
    headers
        .into_iter()
        .filter_map(|h| {
            let (n, v) = h.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
        .collect()
}

fn authserv_id(value: &str) -> &str {
    value.split(';').next().unwrap_or_default().trim()
}

fn tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|t| !t.is_empty())
}

/// The result word that starts a method's result ("fail" of "fail (p=…)").
fn word(s: &str) -> String {
    tokens(s).next().unwrap_or_default().to_lowercase()
}

/// Links whose text shows one site and lead to another, but for those
/// leading to the sender's own domain (its click tracking): `(shown, goes
/// to)`.
pub fn misleading_links(links: &[Link], sender_domain: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for link in links {
        let Some(shown) = &link.shown else { continue };
        let to = registrable(&link.host);
        let pair = (registrable(shown), to.clone());
        if pair.0 != pair.1 && Some(to.as_str()) != sender_domain && !out.contains(&pair) {
            out.push(pair);
        }
    }
    out
}

/// Whether this is the first email from the address's domain, with enough
/// history stored to tell (with little, every domain is new).
pub async fn first_from_domain(pool: &SqlitePool, address: &str, before: i64) -> Result<bool> {
    let Some((_, host)) = address.rsplit_once('@') else {
        return Ok(false);
    };
    let (history, oldest): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), MIN(posted_at) FROM messages WHERE posted_at < ?")
            .bind(before)
            .fetch_one(pool)
            .await
            .context("measuring history")?;
    if history < HISTORY_MESSAGES || oldest.is_none_or(|t| t > before - HISTORY_DAYS * 86_400) {
        return Ok(false);
    }
    let domain = registrable(&host.to_lowercase());
    let (earlier,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM messages m JOIN people p ON p.id = m.author_id \
         WHERE (p.external_id LIKE '%@' || ?1 OR p.external_id LIKE '%.' || ?1) \
         AND m.posted_at < ?2",
    )
    .bind(&domain)
    .bind(before)
    .fetch_one(pool)
    .await
    .context("loading sender history")?;
    Ok(earlier == 0)
}

/// Whether a sender's name reads like someone other than its address's
/// host: none of its words (of four letters or more) appears in the host,
/// as "Bradesco Net Empresas" from `suporte5.ux6qrc.com` does, nor does the
/// registered domain appear in the name ("SpaceXAI" from `x.ai`).
pub fn name_unrelated_to_host(name: &str, host: &str) -> bool {
    let fold = |s: &str| -> String {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
            .collect()
    };
    if name.contains('@') {
        return false;
    }
    let domain: String = fold(&registrable(host)).split_whitespace().collect();
    let host: String = fold(host).split_whitespace().collect();
    let name = fold(name);
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.len() >= 4).collect();
    let joined: String = name.split_whitespace().collect();
    !words.is_empty()
        && !words.iter().any(|w| host.contains(w))
        && !host.contains(&joined)
        && !joined.contains(&domain)
}

/// What [`reasons`] looks at, for one message.
pub struct Facts<'a> {
    pub posted_at: i64,
    pub received_at: Option<i64>,
    pub auth: Option<&'a Auth>,
    pub links: &'a [Link],
    pub author_address: Option<&'a str>,
    pub author_name: Option<&'a str>,
}

/// Every sign the message isn't from who it claims, as reasons to show.
pub async fn reasons(pool: &SqlitePool, facts: &Facts<'_>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    out.extend(super::misdated(facts.posted_at, facts.received_at));
    out.extend(facts.auth.and_then(Auth::failure));
    let host = facts
        .author_address
        .and_then(|a| a.rsplit_once('@'))
        .map(|(_, h)| h.to_lowercase());
    let domain = host.as_deref().map(registrable);
    let misleading = misleading_links(facts.links, domain.as_deref());
    if !misleading.is_empty() {
        let named: Vec<String> = misleading
            .iter()
            .take(NAMED_LINKS)
            .map(|(shown, to)| format!("{shown} leads to {to}"))
            .collect();
        out.push(format!("a link showing {}", named.join(", one showing ")));
    }
    if let (Some(address), Some(name), Some(host), Some(domain)) =
        (facts.author_address, facts.author_name, &host, &domain)
        && name_unrelated_to_host(name, host)
        && first_from_domain(pool, address, facts.posted_at).await?
    {
        out.push(format!(
            "the first email from {domain}, under the name \"{name}\""
        ));
    }
    Ok(out)
}

/// [`reasons`] for each of `ids` that has any; the user's own mail has none.
pub async fn for_messages(
    pool: &SqlitePool,
    ids: &[i64],
    own: &[String],
) -> Result<HashMap<i64, Vec<String>>> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        i64,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT m.id, m.posted_at, m.received_at, m.auth_json, m.links_json, \
                p.external_id, p.display_name \
         FROM messages m LEFT JOIN people p ON p.id = m.author_id \
         WHERE m.id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(ids)?)
    .fetch_all(pool)
    .await
    .context("loading what tells forged mail")?;
    let mut out = HashMap::new();
    for (id, posted_at, received_at, auth, links, address, name) in rows {
        if address
            .as_deref()
            .is_some_and(|a| own.contains(&a.to_lowercase()))
        {
            continue;
        }
        let auth: Option<Auth> = auth.and_then(|a| serde_json::from_str(&a).ok());
        let links: Vec<Link> = links
            .and_then(|l| serde_json::from_str(&l).ok())
            .unwrap_or_default();
        let found = reasons(
            pool,
            &Facts {
                posted_at,
                received_at,
                auth: auth.as_ref(),
                links: &links,
                author_address: address.as_deref(),
                author_name: name.as_deref(),
            },
        )
        .await?;
        if !found.is_empty() {
            out.insert(id, found);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUSIGN: &str = "Received: from x\r\n\
        Authentication-Results: phl-mx-01.messagingengine.com;\r\n    x-csa=none\r\n\
        Authentication-Results: phl-mx-01.messagingengine.com;\r\n    arc=none (no signatures found)\r\n\
        Authentication-Results: phl-mx-01.messagingengine.com;\r\n    dkim=none;\r\n    \
        dmarc=fail policy.published-domain-policy=reject\r\n      (p=reject,d=quarantine) \
        header.from=docusign.net;\r\n    spf=fail smtp.mailfrom=x.example\r\n\
        Authentication-Results: mx.docusign.net; dmarc=pass header.from=docusign.net\r\n\
        Subject: Contract\r\n\r\nbody dmarc=pass";

    #[test]
    fn reads_the_receiving_servers_dmarc_verdict() {
        let auth = Auth::from_headers(DOCUSIGN.as_bytes());
        assert_eq!(
            auth,
            Auth {
                dmarc: Some("fail".into()),
                from: Some("docusign.net".into()),
                mailing_list: false,
                arc: Some("none".into()),
            },
            "the forged header from another server and the body don't count"
        );
        assert_eq!(
            auth.failure().as_deref(),
            Some("docusign.net didn't authorize its sender (DMARC failed)")
        );
        assert_eq!(Auth::from_headers(b"Subject: x\r\n\r\n"), Auth::default());
    }

    #[test]
    fn lists_and_relays_explain_a_dmarc_failure() {
        let list = Auth {
            dmarc: Some("fail".into()),
            mailing_list: true,
            ..Default::default()
        };
        let forwarded = Auth {
            dmarc: Some("fail".into()),
            arc: Some("fail".into()),
            ..Default::default()
        };
        assert_eq!((list.failure(), forwarded.failure()), (None, None));
    }

    #[test]
    fn links_to_another_site_than_shown_mislead_unless_they_are_the_senders() {
        let link = |host: &str, shown: &str| Link {
            host: host.into(),
            shown: Some(shown.into()),
        };
        let links = [
            link("click.voeazul-news.com.br", "www.voeazul.com.br"),
            link("phish.example", "paypal.com"),
            link("www.paypal.com", "paypal.com"),
        ];
        assert_eq!(
            misleading_links(&links, Some("voeazul-news.com.br")),
            [("paypal.com".to_string(), "phish.example".to_string())]
        );
    }

    #[test]
    fn a_name_unrelated_to_its_host_reads_like_an_impostor() {
        assert!(name_unrelated_to_host(
            "Bradesco Net Empresas",
            "suporte5.ux6qrc.com"
        ));
        assert!(!name_unrelated_to_host("Banco Inter", "inter.co"));
        assert!(!name_unrelated_to_host("SpaceXAI", "x.ai"));
    }
}
