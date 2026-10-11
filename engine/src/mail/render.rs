//! Turning a cached message into a page the apps can show in a web view.
//! The HTML is cleaned (no scripts, event handlers, forms or frames), its
//! embedded images (`cid:`) are inlined, and the page carries a Content
//! Security Policy that lets nothing load from the network unless the user
//! asks for remote images; even then, never scripts. Plain-text mail becomes
//! HTML with its links made clickable and its quotes set apart.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use base64::Engine as _;
use chrono::Utc;
use mail_parser::{MessageParser, MimeHeaders, PartType};
use sqlx::SqlitePool;

/// A message ready to show.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// A whole HTML document, policy and base styles included.
    pub html: String,
    /// Remote images (and other remote resources) the policy is holding
    /// back: worth offering "Load images" only when this isn't zero.
    pub remote_resources: u32,
    pub attachments: Vec<Attachment>,
    /// False when the full message isn't cached and this is the stored text
    /// alone: fetch the thread and render again.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    /// The part's position in the message, to fetch it by.
    pub part: u32,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
}

impl From<Rendered> for mnemis_types::RenderedMessageDto {
    fn from(r: Rendered) -> Self {
        Self {
            html: r.html,
            remote_resources: r.remote_resources,
            attachments: r
                .attachments
                .into_iter()
                .map(|a| mnemis_types::AttachmentDto {
                    part: a.part,
                    name: a.name,
                    mime_type: a.mime_type,
                    size: a.size,
                })
                .collect(),
            complete: r.complete,
        }
    }
}

/// Render the message stored as `message_id`: from the cache when it's
/// there (and note that it was read, which keeps it cached), else from its
/// stored text.
pub async fn render_message(
    pool: &SqlitePool,
    message_id: i64,
    allow_remote: bool,
) -> Result<Rendered> {
    let (source_id, external_id, body): (i64, String, String) = sqlx::query_as(
        "SELECT c.source_id, m.external_id, m.body \
         FROM messages m JOIN channels c ON c.id = m.channel_id WHERE m.id = ?",
    )
    .bind(message_id)
    .fetch_one(pool)
    .await
    .context("loading the message")?;
    let mut conn = pool.acquire().await?;
    match super::cache::get(&mut conn, source_id, &external_id).await? {
        Some(raw) => {
            sqlx::query(
                "UPDATE cache.mail_raw SET viewed_at = ? WHERE source_id = ? AND external_id = ?",
            )
            .bind(Utc::now().timestamp())
            .bind(source_id)
            .bind(&external_id)
            .execute(&mut *conn)
            .await?;
            Ok(render(&raw, allow_remote))
        }
        None => Ok(Rendered {
            html: document(&text_to_html(&body), false, false),
            remote_resources: 0,
            attachments: Vec::new(),
            complete: false,
        }),
    }
}

/// One attachment of a cached message (see [`Rendered::attachments`]):
/// its description and contents.
pub async fn attachment(
    pool: &SqlitePool,
    message_id: i64,
    part: u32,
) -> Result<(Attachment, Vec<u8>)> {
    let (source_id, external_id): (i64, String) = sqlx::query_as(
        "SELECT c.source_id, m.external_id \
         FROM messages m JOIN channels c ON c.id = m.channel_id WHERE m.id = ?",
    )
    .bind(message_id)
    .fetch_one(pool)
    .await
    .context("loading the message")?;
    let mut conn = pool.acquire().await?;
    let raw = super::cache::get(&mut conn, source_id, &external_id)
        .await?
        .context("the message isn't downloaded yet")?;
    let found = render(&raw, false)
        .attachments
        .into_iter()
        .find(|a| a.part == part)
        .context("no such attachment")?;
    let msg = MessageParser::default()
        .parse(&raw)
        .context("unreadable message")?;
    let data = msg
        .parts
        .get(part as usize)
        .map(|p| p.contents().to_vec())
        .context("no such attachment")?;
    Ok((found, data))
}

/// Render a whole RFC 822 message.
pub fn render(raw: &[u8], allow_remote: bool) -> Rendered {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return Rendered {
            html: document(&text_to_html(&String::from_utf8_lossy(raw)), false, false),
            remote_resources: 0,
            attachments: Vec::new(),
            complete: true,
        };
    };

    let is_html = has_html_part(&msg);
    let mut inline: HashMap<String, String> = HashMap::new();
    let mut attachments = Vec::new();
    for (index, part) in msg.parts.iter().enumerate() {
        let (PartType::Binary(data) | PartType::InlineBinary(data)) = &part.body else {
            continue;
        };
        let mime_type = part
            .content_type()
            .map(|t| match t.subtype() {
                Some(sub) => format!("{}/{}", t.ctype(), sub),
                None => t.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".to_string())
            .to_lowercase();
        let is_attachment = part
            .content_disposition()
            .is_some_and(|d| d.is_attachment());
        match part.content_id() {
            // Only HTML can show an image in place.
            Some(cid) if is_html && !is_attachment && mime_type.starts_with("image/") => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(data.as_ref());
                inline.insert(
                    cid.trim_matches(['<', '>']).to_string(),
                    format!("data:{mime_type};base64,{encoded}"),
                );
            }
            _ => attachments.push(Attachment {
                part: index as u32,
                name: part.attachment_name().unwrap_or("Attachment").to_string(),
                mime_type,
                size: data.len() as u64,
            }),
        }
    }

    let (html, remote_resources) = match msg.body_html(0) {
        Some(html) if is_html => clean_html(&html, inline),
        _ => {
            let text = msg.body_text(0).map(|t| t.into_owned()).unwrap_or_default();
            (text_to_html(&text), 0)
        }
    };
    Rendered {
        html: document(&html, is_html, allow_remote),
        remote_resources,
        attachments,
        complete: true,
    }
}

/// Whether the message's first body part is real HTML (mail_parser also
/// converts text-only mail to HTML for `body_html`).
fn has_html_part(msg: &mail_parser::Message) -> bool {
    msg.html_body
        .first()
        .and_then(|i| msg.parts.get(*i as usize))
        .is_some_and(|p| matches!(p.body, PartType::Html(_)))
}

/// The cleaned HTML and how many remote resources it references.
fn clean_html(html: &str, inline: HashMap<String, String>) -> (String, u32) {
    let remote = Arc::new(AtomicU32::new(0));
    let counter = remote.clone();
    let inline = Arc::new(inline);
    let clean = ammonia::Builder::default()
        // Mail's look lives in stylesheets and presentational attributes.
        .rm_clean_content_tags(&["style"])
        .add_tags(&["style", "font", "center"])
        .add_generic_attributes(&[
            "style",
            "class",
            "align",
            "valign",
            "width",
            "height",
            "bgcolor",
            "color",
            "background",
            "border",
            "cellpadding",
            "cellspacing",
            "dir",
            "face",
            "size",
        ])
        .add_url_schemes(&["cid", "data"])
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .attribute_filter(move |element, attribute, value| {
            // The filter also sees elements ammonia goes on to drop (frames,
            // media): only images and backgrounds survive to load anything.
            let is_url = (element == "img" && matches!(attribute, "src" | "srcset"))
                || attribute == "background";
            if is_url {
                if let Some(cid) = value.strip_prefix("cid:") {
                    return inline
                        .get(cid.trim_matches(['<', '>']))
                        .map(|d| Cow::Owned(d.clone()));
                }
                if is_remote(value) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            } else if attribute == "style" {
                counter.fetch_add(remote_urls_in_css(value), Ordering::Relaxed);
            }
            Some(value.into())
        })
        .clean(html)
        .to_string();
    // Stylesheets aren't attributes: count their remote URLs separately.
    let in_sheets = clean
        .split("<style")
        .skip(1)
        .filter_map(|s| s.split_once("</style>").map(|(css, _)| css))
        .map(remote_urls_in_css)
        .sum::<u32>();
    (clean, remote.load(Ordering::Relaxed) + in_sheets)
}

fn is_remote(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("http:") || url.starts_with("https:") || url.starts_with("//")
}

fn remote_urls_in_css(css: &str) -> u32 {
    let lower = css.to_ascii_lowercase();
    let urls = lower
        .split("url(")
        .skip(1)
        .filter(|rest| is_remote(rest.trim_start_matches(['"', '\'', ' '])))
        .count();
    let imports = lower.matches("@import").count();
    (urls + imports) as u32
}

/// Plain text as HTML: escaped, links made clickable, quoted lines set
/// apart, line breaks kept.
fn text_to_html(text: &str) -> String {
    let mut out = String::from("<div class=\"plain\">");
    let mut depth = 0;
    for line in text.lines() {
        let mut level = 0;
        let mut rest = line;
        while let Some(r) = rest.strip_prefix('>') {
            level += 1;
            rest = r.strip_prefix(' ').unwrap_or(r);
        }
        while depth < level {
            out.push_str("<blockquote>");
            depth += 1;
        }
        while depth > level {
            out.push_str("</blockquote>");
            depth -= 1;
        }
        out.push_str(&linkify(rest));
        out.push_str("<br>");
    }
    for _ in 0..depth {
        out.push_str("</blockquote>");
    }
    out.push_str("</div>");
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Escape a line, turning http(s) URLs into links.
fn linkify(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|p| rest.find(p))
        .min()
    {
        out.push_str(&escape(&rest[..start]));
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
            .unwrap_or(tail.len());
        // Sentence punctuation after a link isn't part of it.
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        out.push_str(&format!(
            "<a href=\"{0}\" target=\"_blank\" rel=\"noopener noreferrer\">{0}</a>",
            escape(url)
        ));
        rest = &tail[url.len()..];
    }
    out.push_str(&escape(rest));
    out
}

/// The page around a message body. HTML mail is designed for a light page,
/// so it gets one; plain text follows the system's light or dark look.
fn document(body: &str, is_html: bool, allow_remote: bool) -> String {
    let images = if allow_remote {
        "data: https: http:"
    } else {
        "data:"
    };
    // HTML mail assumes a white page; the frame is transparent otherwise.
    let (scheme, page) = if is_html {
        ("light", "html{background:#fff;color:#000}")
    } else {
        ("light dark", "")
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         img-src {images}; style-src 'unsafe-inline'; font-src data:\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <style>:root{{color-scheme:{scheme}}}{page}\
         body{{margin:0;padding:12px 16px;font:15px/1.45 -apple-system,system-ui,sans-serif;\
         overflow-wrap:anywhere}}\
         img{{max-width:100%;height:auto}}\
         .plain blockquote{{margin:4px 0 4px 2px;padding-left:10px;border-left:2px solid #8884;opacity:.8}}\
         </style></head><body>{body}</body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html_mail(html: &str) -> Vec<u8> {
        format!(
            "Message-ID: <h@x>\r\nSubject: Hi\r\nMIME-Version: 1.0\r\n\
             Content-Type: text/html; charset=utf-8\r\n\r\n{html}"
        )
        .into_bytes()
    }

    #[test]
    fn html_mail_loses_scripts_and_holds_back_remote_images() {
        let r = render(
            &html_mail(
                "<style>p{color:red} .x{background:url(https://t.example/a.png)}</style>\
                 <p onclick=\"steal()\" style=\"color:blue\">Hello</p>\
                 <script>steal()</script><img src=\"https://t.example/pixel.gif\">\
                 <form action=\"https://evil\"><input name=pw></form>\
                 <a href=\"https://example.com\">link</a>\
                 <meta http-equiv=\"refresh\" content=\"0;url=https://t.example/\">\
                 <iframe src=\"https://t.example/\"></iframe>",
            ),
            false,
        );
        assert!(r.complete);
        assert!(!r.html.contains("steal"), "scripts and handlers are gone");
        assert!(!r.html.contains("<form") && !r.html.contains("<input"));
        assert!(
            !r.html.contains("refresh") && !r.html.contains("<iframe"),
            "no navigation"
        );
        assert!(r.html.contains("p{color:red}"), "stylesheets stay");
        assert!(r.html.contains("style=\"color:blue\""));
        assert!(
            r.html.contains("img-src data:;"),
            "remote images blocked by policy"
        );
        assert!(r.html.contains("target=\"_blank\""));
        assert_eq!(
            r.remote_resources, 2,
            "the pixel and the stylesheet's image"
        );

        let shown = render(
            &html_mail("<img src=\"https://t.example/pixel.gif\">"),
            true,
        );
        assert!(shown.html.contains("img-src data: https: http:"));
        assert!(
            !shown.html.contains("script-src"),
            "never scripts, even then"
        );
    }

    #[test]
    fn embedded_images_are_inlined_and_files_are_listed() {
        let raw = "Message-ID: <m@x>\r\nSubject: Photo\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
             --b1\r\nContent-Type: multipart/related; boundary=\"b2\"\r\n\r\n\
             --b2\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
             <p>Look</p><img src=\"cid:pic@x\">\r\n\
             --b2\r\nContent-Type: image/png\r\nContent-ID: <pic@x>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n--b2--\r\n\
             --b1\r\nContent-Type: application/pdf; name=\"contract.pdf\"\r\n\
             Content-Disposition: attachment; filename=\"contract.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n--b1--\r\n";
        let r = render(raw.as_bytes(), false);
        assert!(
            r.html
                .contains("src=\"data:image/png;base64,iVBORw0KGgo=\"")
        );
        assert_eq!(r.remote_resources, 0);
        assert_eq!(r.attachments.len(), 1);
        assert_eq!(r.attachments[0].name, "contract.pdf");
        assert_eq!(r.attachments[0].mime_type, "application/pdf");
        let msg = MessageParser::default().parse(raw.as_bytes()).unwrap();
        assert_eq!(
            msg.parts[r.attachments[0].part as usize].contents(),
            b"%PDF-"
        );
    }

    #[test]
    fn plain_text_gets_links_and_quotes() {
        let raw = "Message-ID: <t@x>\r\nSubject: Re: Hi\r\n\r\n\
                   See https://example.com/a?b=1&c=2.\r\n\r\n> earlier <words>\r\n>> older\r\n";
        let r = render(raw.as_bytes(), false);
        assert!(r.html.contains(
            "<a href=\"https://example.com/a?b=1&amp;c=2\" target=\"_blank\" rel=\"noopener noreferrer\">"
        ));
        assert!(
            r.html.contains("</a>.<br>"),
            "the full stop isn't part of the link"
        );
        assert!(
            r.html
                .contains("<blockquote>earlier &lt;words&gt;<br><blockquote>older")
        );
        assert!(r.html.contains("color-scheme:light dark"));
    }
}
