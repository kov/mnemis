//! Where a mail's links lead, for phishing signals: each link's destination
//! host and, when its visible text is itself a URL, the host that text shows.
//! A scan of the HTML good enough for mail, not a full parser.

use serde::{Deserialize, Serialize};

/// Most links kept per message; newsletters can carry hundreds.
const MAX_LINKS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Host the link goes to.
    pub host: String,
    /// Host shown by the link's text, when the text is a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<String>,
}

/// The distinct links of a parsed message: the anchors of its HTML parts or,
/// for plain-text mail, the URLs in its text.
pub fn from_message(msg: &mail_parser::Message<'_>) -> Vec<Link> {
    use mail_parser::PartType;
    let mut links = Vec::new();
    for part in msg
        .html_body
        .iter()
        .filter_map(|i| msg.parts.get(*i as usize))
    {
        match &part.body {
            PartType::Html(html) => links.extend(anchors(html)),
            PartType::Text(text) => links.extend(text_urls(text)),
            _ => {}
        }
    }
    if msg.html_body.is_empty() {
        for part in msg
            .text_body
            .iter()
            .filter_map(|i| msg.parts.get(*i as usize))
        {
            if let PartType::Text(text) = &part.body {
                links.extend(text_urls(text));
            }
        }
    }
    let mut out: Vec<Link> = Vec::new();
    for link in links {
        if !out.contains(&link) {
            out.push(link);
            if out.len() == MAX_LINKS {
                break;
            }
        }
    }
    out
}

/// `<a href="…">text</a>` pairs.
fn anchors(html: &str) -> Vec<Link> {
    // ASCII lowercasing keeps byte offsets, so positions found in `lower`
    // index `html` too.
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(found) = lower[pos..].find("<a") {
        let start = pos + found;
        pos = start + 2;
        if !lower[pos..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(tag_end) = tag_end(&lower, pos) else {
            break;
        };
        let href = attribute(&html[pos..tag_end], &lower[pos..tag_end], "href");
        pos = tag_end + 1;
        let text_end = lower[pos..].find("</a").map_or(lower.len(), |i| pos + i);
        let text = visible_text(&html[pos..text_end]);
        pos = text_end;
        if let Some(host) = href.as_deref().and_then(host_of) {
            out.push(Link {
                host,
                shown: shown_host(&text),
            });
        }
    }
    out
}

/// The `>` closing a tag that starts before `from`, skipping quoted values.
fn tag_end(lower: &str, from: usize) -> Option<usize> {
    let mut quote = None;
    for (i, c) in lower[from..].char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(from + i),
            _ => {}
        }
    }
    None
}

/// An attribute's value within a tag's attribute text.
fn attribute(tag: &str, lower: &str, name: &str) -> Option<String> {
    let mut pos = 0;
    while let Some(found) = lower[pos..].find(name) {
        let at = pos + found;
        pos = at + name.len();
        let boundary = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = lower[pos..].trim_start();
        if !boundary || !rest.starts_with('=') {
            continue;
        }
        let value_at = lower.len() - rest.len() + 1;
        let value = tag[value_at..].trim_start();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
            _ => value
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or(""),
        };
        return Some(decode_entities(value.trim()));
    }
    None
}

/// An element's text without its tags, entities decoded, whitespace
/// collapsed.
fn visible_text(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&#47;", "/")
        .replace("&#x2F;", "/")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The lowercase host of an absolute web URL; None for mailto:, anchors,
/// relative links and the like. Userinfo is dropped, so
/// `https://bank.com@evil.example/` yields `evil.example`.
fn host_of(url: &str) -> Option<String> {
    let lower = url.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .or_else(|| lower.strip_prefix("//"))?;
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?.trim_end_matches('.');
    (host.contains('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'))
    .then(|| host.to_string())
}

/// The host a link's text displays, when the text reads as a URL or a bare
/// domain ("www.bank.com", "bank.com/login").
fn shown_host(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches(['.', ',', ';', ':', ')', '!']);
    if text.is_empty() || text.contains(char::is_whitespace) || text.contains('@') {
        return None;
    }
    if let Some(host) = host_of(text) {
        return Some(host);
    }
    // A bare word with a dot ("Mr.Smith") isn't a domain: without "www.",
    // want a common top-level label.
    const TLDS: [&str; 20] = [
        "com", "net", "org", "br", "io", "co", "me", "uk", "us", "de", "fr", "pt", "es", "it",
        "info", "app", "dev", "ai", "gov", "edu",
    ];
    let host = host_of(&format!("http://{text}"))?;
    let tld = host.rsplit('.').next()?;
    (host.starts_with("www.") || TLDS.contains(&tld)).then_some(host)
}

/// URLs written out in plain text.
fn text_urls(text: &str) -> Vec<Link> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '(' | ')'))
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .filter_map(host_of)
        .map(|host| Link { host, shown: None })
        .collect()
}

/// The domain an organisation registered, roughly: the host without its
/// subdomains, keeping country and generic suffixes ("mail.itau.com.br" is
/// "itau.com.br").
pub fn registrable(host: &str) -> String {
    const SUFFIXES: [&str; 12] = [
        "com", "net", "org", "gov", "edu", "co", "ac", "br", "uk", "au", "jp", "nz",
    ];
    let host = host.trim().trim_end_matches('.').to_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    // The top-level domain, then any generic labels before it ("com.br").
    let mut suffix = labels.len().saturating_sub(1);
    while suffix > 0 && SUFFIXES.contains(&labels[suffix - 1]) {
        suffix -= 1;
    }
    labels[suffix.saturating_sub(1)..].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(host: &str, shown: Option<&str>) -> Link {
        Link {
            host: host.to_string(),
            shown: shown.map(str::to_string),
        }
    }

    #[test]
    fn reads_anchor_hosts_and_the_urls_they_show() {
        let html = r##"<p>Hi</p>
            <A class="btn" HREF="https://click.evil.example/x?u=1">ATUALIZAR</a>
            <a href='https://www.spotify.com.evil.example/login'><b>www.spotify.com</b></a>
            <a title="x>y" href=https://paypal.com@phish.example/>https://paypal.com/</a>
            <a href="mailto:help@spotify.com">help</a>
            <a href="#top">top</a>
            <a href="https://Spotify.com/account">Mr.Smith</a>"##;
        assert_eq!(
            anchors(html),
            vec![
                link("click.evil.example", None),
                link("www.spotify.com.evil.example", Some("www.spotify.com")),
                link("phish.example", Some("paypal.com")),
                link("spotify.com", None),
            ]
        );
    }

    #[test]
    fn finds_urls_in_plain_text() {
        assert_eq!(
            text_urls("Pay at https://pay.example.com/x, or (https://b.example) now"),
            vec![link("pay.example.com", None), link("b.example", None)]
        );
    }

    #[test]
    fn keeps_the_registered_domain() {
        assert_eq!(registrable("mail.itau.com.br"), "itau.com.br");
        assert_eq!(registrable("em1.cloudflare.com"), "cloudflare.com");
        assert_eq!(registrable("rejanetoigo.com"), "rejanetoigo.com");
        assert_eq!(registrable("vxadv-22.srv.cat"), "srv.cat");
        assert_eq!(registrable("x.ai"), "x.ai");
    }
}
