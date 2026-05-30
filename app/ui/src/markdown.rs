//! Render an assistant reply (CommonMark) to a *sanitized* HTML fragment.
//!
//! The model frequently answers in markdown, and its input includes the
//! user's ingested mail/chat — which an attacker could lace with markup or
//! `javascript:` links. So we never trust the output: raw HTML events are
//! dropped, links are restricted to a small scheme allowlist, and images are
//! removed entirely (keeping their alt text) so no remote resource is fetched.
//! Everything that survives goes through `pulldown_cmark`'s HTML writer, which
//! escapes text and attribute values.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

/// Schemes we'll render as real links (and hand to the OS opener on click).
pub fn is_safe_href(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// Convert a markdown string into a sanitized HTML fragment safe for
/// `inner_html` inside an assistant bubble.
pub fn markdown_to_html(md: &str) -> String {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;

    // Links and images come as Start/End pairs wrapping their visible content.
    // To drop the wrapper while keeping that content, we suppress only the
    // tags themselves and let the inner events through.
    let mut suppressed_links = 0u32;
    let mut suppressed_images = 0u32;

    let events: Vec<Event> = Parser::new_ext(md, options)
        .filter_map(|event| match event {
            // Raw HTML (block or inline) is never emitted verbatim.
            Event::Html(_) | Event::InlineHtml(_) => None,

            // Links with a disallowed scheme degrade to plain text.
            Event::Start(Tag::Link { dest_url, .. }) if !is_safe_href(&dest_url) => {
                suppressed_links += 1;
                None
            }
            Event::End(TagEnd::Link) if suppressed_links > 0 => {
                suppressed_links -= 1;
                None
            }

            // Images are dropped wholesale; their alt text renders inline.
            Event::Start(Tag::Image { .. }) => {
                suppressed_images += 1;
                None
            }
            Event::End(TagEnd::Image) if suppressed_images > 0 => {
                suppressed_images -= 1;
                None
            }

            other => Some(other),
        })
        .collect();

    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    out
}
