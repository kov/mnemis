//! Hand-rolled CalDAV client for the VTODO task backend.
//!
//! We only need a thin slice of RFC 4791: discover the task collection, list
//! VTODOs with their ETags, and create/update/delete a resource with optimistic
//! concurrency. That's a handful of `PROPFIND`/`REPORT`/`PUT`/`DELETE` requests
//! over the engine's existing reqwest client — no second HTTP stack (see the
//! `v2-redesign` memory for why `libdav` was rejected). The iCalendar bodies are
//! built/parsed by [`super::vtodo`]; the XML multistatus responses are parsed
//! here with `roxmltree`, matching on **local** element names so namespace
//! prefixes (which differ across servers) don't matter.
//!
//! Discovery follows the standard chain: `current-user-principal` →
//! `calendar-home-set` → list the home's child collections and keep the ones
//! whose `supported-calendar-component-set` advertises `VTODO`. Servers return
//! hrefs as absolute paths, so each is resolved against the URL that produced
//! it (which, after iCloud's redirect to a shard host, is the real origin).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, ETAG};
use reqwest::{Client, Method, StatusCode, Url};

use super::vtodo;
use super::{Conditional, Created, RemoteTask, TaskBackend, TaskStatus, TaskWrite};

const XML_CT: &str = "application/xml; charset=utf-8";
const ICS_CT: &str = "text/calendar; charset=utf-8";

const PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;

const HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;

const COLLECTIONS_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:displayname/><d:resourcetype/><c:supported-calendar-component-set/></d:prop></d:propfind>"#;

const GETETAG_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>"#;

const CALENDAR_QUERY_VTODO: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter></c:calendar-query>"#;

/// A task collection found on the server, ready to show in settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCollection {
    /// Absolute URL of the collection, used verbatim as the backend's binding.
    pub url: String,
    pub display_name: Option<String>,
}

/// A CalDAV backend bound to one already-known task collection. Construct via
/// [`CaldavBackend::new`] once the collection URL is known (from discovery or a
/// manually-entered URL).
pub struct CaldavBackend {
    http: Client,
    collection: Url,
    username: String,
    password: String,
}

impl CaldavBackend {
    pub fn new(collection_url: &str, username: &str, password: &str) -> Result<Self> {
        Ok(Self {
            http: build_client()?,
            collection: Url::parse(collection_url)
                .with_context(|| format!("parsing CalDAV collection URL {collection_url}"))?,
            username: username.to_owned(),
            password: password.to_owned(),
        })
    }

    /// Re-read a single resource's ETag — a fallback for servers that don't echo
    /// `ETag` on a `PUT` response.
    async fn refetch_etag(&self, url: &Url) -> Result<String> {
        let resp = self
            .request(
                b"PROPFIND",
                url,
                Some("0"),
                Some(XML_CT),
                Some(GETETAG_BODY.into()),
                &[],
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await.context("reading getetag body")?;
        if !status.is_success() {
            bail!("CalDAV PROPFIND getetag {url} failed (HTTP {status})");
        }
        first_text(&text, "getetag").ok_or_else(|| anyhow!("no getetag in response for {url}"))
    }

    /// Send a CalDAV request with Basic auth and the given headers. `extra` is a
    /// list of `(name, value)` header pairs (Depth, If-Match, If-None-Match).
    async fn request(
        &self,
        method: &[u8],
        url: &Url,
        depth: Option<&str>,
        content_type: Option<&str>,
        body: Option<String>,
        extra: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        caldav_request(
            &self.http,
            method,
            url,
            &self.username,
            &self.password,
            depth,
            content_type,
            body,
            extra,
        )
        .await
    }
}

#[async_trait]
impl TaskBackend for CaldavBackend {
    async fn list_tasks(&self) -> Result<Vec<RemoteTask>> {
        let resp = self
            .request(
                b"REPORT",
                &self.collection,
                Some("1"),
                Some(XML_CT),
                Some(CALENDAR_QUERY_VTODO.into()),
                &[],
            )
            .await?;
        let status = resp.status();
        let base = resp.url().clone();
        let text = resp.text().await.context("reading REPORT body")?;
        if !status.is_success() {
            bail!("CalDAV REPORT {} failed (HTTP {status})", self.collection);
        }

        let mut out = Vec::new();
        for raw in parse_report_tasks(&text) {
            let Some(parsed) = vtodo::parse_vtodo(&raw.calendar_data) else {
                continue;
            };
            let Some(uid) = parsed.uid else {
                // A VTODO without a UID can't be matched to an action; skip it
                // rather than inventing identity.
                continue;
            };
            let href = base
                .join(&raw.href)
                .map(|u| u.to_string())
                .unwrap_or(raw.href);
            out.push(RemoteTask {
                uid,
                href,
                etag: raw.etag,
                summary: parsed.summary.unwrap_or_default(),
                description: parsed.description,
                due: parsed.due,
                status: parsed.status.unwrap_or(TaskStatus::NeedsAction),
            });
        }
        Ok(out)
    }

    async fn create_task(&self, task: &TaskWrite) -> Result<Created> {
        let resource = self
            .collection
            .join(&resource_name(&task.uid))
            .with_context(|| format!("building resource URL under {}", self.collection))?;
        let body = vtodo::task_to_ics(task);
        // If-None-Match: * makes the PUT a pure create — fail rather than
        // clobber if the name already exists.
        let resp = self
            .request(
                b"PUT",
                &resource,
                None,
                Some(ICS_CT),
                Some(body),
                &[("If-None-Match", "*")],
            )
            .await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("CalDAV PUT (create) {resource} failed (HTTP {status})");
        }
        let etag = match etag_header(&resp) {
            Some(e) => e,
            None => self.refetch_etag(&resource).await.unwrap_or_default(),
        };
        Ok(Created {
            href: resource.to_string(),
            etag,
        })
    }

    async fn update_task(
        &self,
        href: &str,
        etag: &str,
        task: &TaskWrite,
    ) -> Result<Conditional<String>> {
        let url = Url::parse(href).with_context(|| format!("parsing task href {href}"))?;
        let body = vtodo::task_to_ics(task);
        let resp = self
            .request(
                b"PUT",
                &url,
                None,
                Some(ICS_CT),
                Some(body),
                &[("If-Match", etag)],
            )
            .await?;
        let status = resp.status();
        if status == StatusCode::PRECONDITION_FAILED {
            return Ok(Conditional::Conflict);
        }
        if !status.is_success() {
            bail!("CalDAV PUT (update) {url} failed (HTTP {status})");
        }
        let new_etag = match etag_header(&resp) {
            Some(e) => e,
            None => self.refetch_etag(&url).await.unwrap_or_default(),
        };
        Ok(Conditional::Ok(new_etag))
    }

    async fn delete_task(&self, href: &str, etag: &str) -> Result<Conditional<()>> {
        let url = Url::parse(href).with_context(|| format!("parsing task href {href}"))?;
        let resp = self
            .request(b"DELETE", &url, None, None, None, &[("If-Match", etag)])
            .await?;
        let status = resp.status();
        if status == StatusCode::PRECONDITION_FAILED {
            return Ok(Conditional::Conflict);
        }
        // A 404 means it's already gone — the desired end state, so treat as Ok.
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(Conditional::Ok(()));
        }
        bail!("CalDAV DELETE {url} failed (HTTP {status})");
    }
}

/// Discover the VTODO-capable task collections for an account. Returns every
/// matching collection; the UI lets the user pick (or there's exactly one).
pub async fn discover_task_collections(
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<Vec<DiscoveredCollection>> {
    let http = build_client()?;
    let base = Url::parse(base_url).with_context(|| format!("parsing base URL {base_url}"))?;

    let (after_principal, principal_body) =
        propfind(&http, &base, "0", PRINCIPAL_BODY, username, password).await?;
    let principal_href = first_href_in(&principal_body, "current-user-principal")
        .ok_or_else(|| anyhow!("server returned no current-user-principal"))?;
    let principal = after_principal.join(&principal_href)?;

    let (after_home, home_body) =
        propfind(&http, &principal, "0", HOME_BODY, username, password).await?;
    let home_href = first_href_in(&home_body, "calendar-home-set")
        .ok_or_else(|| anyhow!("server returned no calendar-home-set"))?;
    let home = after_home.join(&home_href)?;

    let (after_collections, collections_body) =
        propfind(&http, &home, "1", COLLECTIONS_BODY, username, password).await?;

    let mut out = Vec::new();
    for raw in parse_collections(&collections_body) {
        if is_task_list(&raw) {
            let url = after_collections.join(&raw.href)?.to_string();
            out.push(DiscoveredCollection {
                url,
                display_name: raw.display_name,
            });
        }
    }
    Ok(out)
}

fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building CalDAV HTTP client")
}

async fn propfind(
    http: &Client,
    url: &Url,
    depth: &str,
    body: &str,
    username: &str,
    password: &str,
) -> Result<(Url, String)> {
    let resp = caldav_request(
        http,
        b"PROPFIND",
        url,
        username,
        password,
        Some(depth),
        Some(XML_CT),
        Some(body.to_owned()),
        &[],
    )
    .await?;
    let final_url = resp.url().clone();
    let status = resp.status();
    let text = resp.text().await.context("reading PROPFIND body")?;
    if !status.is_success() {
        bail!("CalDAV PROPFIND {url} failed (HTTP {status})");
    }
    Ok((final_url, text))
}

#[allow(clippy::too_many_arguments)]
async fn caldav_request(
    http: &Client,
    method: &[u8],
    url: &Url,
    username: &str,
    password: &str,
    depth: Option<&str>,
    content_type: Option<&str>,
    body: Option<String>,
    extra: &[(&str, &str)],
) -> Result<reqwest::Response> {
    let method = Method::from_bytes(method).context("invalid HTTP method")?;
    let mut req = http
        .request(method.clone(), url.clone())
        .basic_auth(username, Some(password));
    if let Some(d) = depth {
        req = req.header("Depth", d);
    }
    if let Some(ct) = content_type {
        req = req.header(CONTENT_TYPE, ct);
    }
    for (name, value) in extra {
        req = req.header(*name, *value);
    }
    if let Some(b) = body {
        req = req.body(b);
    }
    req.send()
        .await
        .with_context(|| format!("CalDAV {method} {url}"))
}

fn etag_header(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// A filesystem-safe `.ics` resource name derived from the UID.
fn resource_name(uid: &str) -> String {
    let slug: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{slug}.ics")
}

// ---- pure XML parsing (unit-tested offline) ------------------------------

/// First text content of any descendant element with the given local name.
fn first_text(xml: &str, local_name: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants()
        .find(|n| n.tag_name().name() == local_name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_owned())
}

/// The first `<href>` nested inside the first element with `container` local
/// name — e.g. the principal href inside `<current-user-principal>`.
fn first_href_in(xml: &str, container: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let node = doc
        .descendants()
        .find(|n| n.tag_name().name() == container)?;
    node.descendants()
        .find(|n| n.tag_name().name() == "href")
        .and_then(|n| n.text())
        .map(|s| s.trim().to_owned())
}

struct RawCollection {
    href: String,
    display_name: Option<String>,
    components: Vec<String>,
    /// Whether `resourcetype` contains a `<C:calendar/>` element. Real calendar
    /// / reminders collections do; the scheduling inbox/outbox (which can still
    /// advertise a `VTODO` component-set) do not — so this is what keeps the
    /// `…/outbox/` collection out of the task-list picker.
    is_calendar: bool,
}

/// A genuine task list: a calendar collection that advertises `VTODO`. The
/// `is_calendar` half excludes iCloud's scheduling inbox/outbox, which would
/// otherwise slip through on the component-set alone.
fn is_task_list(raw: &RawCollection) -> bool {
    raw.is_calendar && raw.components.iter().any(|c| c == "VTODO")
}

/// Parse a Depth:1 PROPFIND multistatus into the child collections, reading
/// each one's displayname, resourcetype, and advertised calendar components.
fn parse_collections(xml: &str) -> Vec<RawCollection> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.tag_name().name() == "response")
        .filter_map(|resp| {
            let href = resp
                .descendants()
                .find(|n| n.tag_name().name() == "href")
                .and_then(|n| n.text())?
                .trim()
                .to_owned();
            let display_name = resp
                .descendants()
                .find(|n| n.tag_name().name() == "displayname")
                .and_then(|n| n.text())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            let components = resp
                .descendants()
                .filter(|n| n.tag_name().name() == "comp")
                .filter_map(|n| n.attribute("name").map(str::to_owned))
                .collect();
            let is_calendar = resp
                .descendants()
                .find(|n| n.tag_name().name() == "resourcetype")
                .map(|rt| rt.descendants().any(|n| n.tag_name().name() == "calendar"))
                .unwrap_or(false);
            Some(RawCollection {
                href,
                display_name,
                components,
                is_calendar,
            })
        })
        .collect()
}

struct RawTask {
    href: String,
    etag: String,
    calendar_data: String,
}

/// Parse a calendar-query REPORT multistatus into (href, etag, VTODO body)
/// triples. Responses missing any of the three are dropped.
fn parse_report_tasks(xml: &str) -> Vec<RawTask> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    doc.descendants()
        .filter(|n| n.tag_name().name() == "response")
        .filter_map(|resp| {
            let text = |local: &str| {
                resp.descendants()
                    .find(|n| n.tag_name().name() == local)
                    .and_then(|n| n.text())
                    .map(|s| s.trim().to_owned())
            };
            Some(RawTask {
                href: text("href")?,
                etag: text("getetag")?,
                calendar_data: text("calendar-data")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_user_principal_href() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/123456/principal/</d:href>
    <d:propstat>
      <d:prop><d:current-user-principal><d:href>/123456/principal/</d:href></d:current-user-principal></d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        assert_eq!(
            first_href_in(xml, "current-user-principal").as_deref(),
            Some("/123456/principal/")
        );
    }

    #[test]
    fn keeps_only_vtodo_calendars_and_excludes_the_scheduling_outbox() {
        // iCloud-shaped Depth:1 home listing: a VEVENT calendar, a VTODO
        // reminders list, and the scheduling outbox — which advertises a VTODO
        // component-set but is NOT a calendar, so it must be filtered out.
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/123/calendars/work/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Work</d:displayname>
      <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/></cal:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/123/calendars/reminders/</d:href>
    <d:propstat><d:prop>
      <d:displayname>Reminders</d:displayname>
      <d:resourcetype><d:collection/><cal:calendar/></d:resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/123/calendar/outbox/</d:href>
    <d:propstat><d:prop>
      <d:resourcetype><d:collection/><cal:schedule-outbox/></d:resourcetype>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/><cal:comp name="VTODO"/></cal:supported-calendar-component-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

        let cols = parse_collections(xml);
        let task_lists: Vec<_> = cols.iter().filter(|c| is_task_list(c)).collect();
        assert_eq!(
            task_lists.len(),
            1,
            "only the reminders list is a task list"
        );
        assert_eq!(task_lists[0].href, "/123/calendars/reminders/");
        assert_eq!(task_lists[0].display_name.as_deref(), Some("Reminders"));

        // The outbox parsed (it has a VTODO comp) but is rejected for not being
        // a calendar — the precise reason it used to leak into the picker.
        let outbox = cols
            .iter()
            .find(|c| c.href == "/123/calendar/outbox/")
            .expect("outbox response should still parse");
        assert!(outbox.components.iter().any(|c| c == "VTODO"));
        assert!(!outbox.is_calendar);
        assert!(!is_task_list(outbox));
    }

    #[test]
    fn parses_report_into_tasks_and_through_vtodo() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/123/calendars/reminders/abc.ics</d:href>
    <d:propstat><d:prop>
      <d:getetag>"etag-xyz"</d:getetag>
      <cal:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Apple//Reminders//EN
BEGIN:VTODO
UID:abc-123
SUMMARY:Pay rent
STATUS:NEEDS-ACTION
END:VTODO
END:VCALENDAR</cal:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

        let tasks = parse_report_tasks(xml);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].href, "/123/calendars/reminders/abc.ics");
        assert_eq!(tasks[0].etag, "\"etag-xyz\"");

        let parsed = vtodo::parse_vtodo(&tasks[0].calendar_data).expect("a VTODO");
        assert_eq!(parsed.uid.as_deref(), Some("abc-123"));
        assert_eq!(parsed.summary.as_deref(), Some("Pay rent"));
        assert_eq!(parsed.status, Some(TaskStatus::NeedsAction));
    }

    #[test]
    fn relative_href_resolves_against_shard_host() {
        // After iCloud's redirect, hrefs are absolute paths to resolve against
        // the shard origin, not the base domain.
        let base = Url::parse("https://p42-caldav.icloud.com/123/calendars/reminders/").unwrap();
        let resolved = base.join("/123/calendars/reminders/abc.ics").unwrap();
        assert_eq!(
            resolved.as_str(),
            "https://p42-caldav.icloud.com/123/calendars/reminders/abc.ics"
        );
    }

    #[test]
    fn resource_name_is_ics_and_safe() {
        assert_eq!(
            resource_name("mnemis-action-7@mnemis"),
            "mnemis-action-7-mnemis.ics"
        );
    }

    /// Live round-trip against a real CalDAV server. Skipped unless
    /// `MNEMIS_TEST_CALDAV=live` plus credentials are set — mirroring the
    /// `MNEMIS_TEST_LLM=live` convention. It is **self-cleaning**: it removes
    /// any leftover test task first and deletes the one it creates, so it
    /// doesn't litter your reminders. It does *not* touch the mnemis database.
    ///
    /// ```text
    /// MNEMIS_TEST_CALDAV=live \
    ///   MNEMIS_TEST_CALDAV_URL=https://caldav.icloud.com \
    ///   MNEMIS_TEST_CALDAV_USER=you@icloud.com \
    ///   MNEMIS_TEST_CALDAV_PASS=app-specific-password \
    ///   [MNEMIS_TEST_CALDAV_COLLECTION=https://.../reminders/] \
    ///   cargo test -p mnemis-engine --lib sync::caldav::tests::live_round_trip -- --nocapture
    /// ```
    #[tokio::test]
    async fn live_round_trip() -> Result<()> {
        if std::env::var("MNEMIS_TEST_CALDAV").as_deref() != Ok("live") {
            eprintln!("skipping live_round_trip (set MNEMIS_TEST_CALDAV=live to run)");
            return Ok(());
        }
        let base = std::env::var("MNEMIS_TEST_CALDAV_URL").expect("MNEMIS_TEST_CALDAV_URL");
        let user = std::env::var("MNEMIS_TEST_CALDAV_USER").expect("MNEMIS_TEST_CALDAV_USER");
        let pass = std::env::var("MNEMIS_TEST_CALDAV_PASS").expect("MNEMIS_TEST_CALDAV_PASS");

        let collections = discover_task_collections(&base, &user, &pass).await?;
        assert!(!collections.is_empty(), "no VTODO collections discovered");
        eprintln!("discovered {} task collection(s):", collections.len());
        for c in &collections {
            eprintln!("  - {} ({:?})", c.url, c.display_name);
        }
        let collection = std::env::var("MNEMIS_TEST_CALDAV_COLLECTION")
            .unwrap_or_else(|_| collections[0].url.clone());
        let backend = CaldavBackend::new(&collection, &user, &pass)?;

        let uid = "mnemis-livetest@mnemis";
        // Clean any leftover from a prior aborted run.
        for t in backend.list_tasks().await? {
            if t.uid == uid {
                backend.delete_task(&t.href, &t.etag).await?;
            }
        }

        let created = backend
            .create_task(&TaskWrite {
                uid: uid.to_string(),
                summary: "mnemis live test — safe to delete".to_string(),
                description: Some("Created by the mnemis live CalDAV test.".to_string()),
                due: Some(1_780_000_000),
                status: TaskStatus::NeedsAction,
            })
            .await?;

        let listed = backend.list_tasks().await?;
        let found = listed
            .iter()
            .find(|t| t.uid == uid)
            .expect("the created task should appear in a fresh list");
        assert_eq!(found.summary, "mnemis live test — safe to delete");
        assert_eq!(found.due, Some(1_780_000_000));

        // Complete it (exercise If-Match update).
        let new_etag = match backend
            .update_task(
                &created.href,
                &found.etag,
                &TaskWrite {
                    uid: uid.to_string(),
                    summary: "mnemis live test — done".to_string(),
                    description: None,
                    due: Some(1_780_000_000),
                    status: TaskStatus::Completed,
                },
            )
            .await?
        {
            Conditional::Ok(e) => e,
            Conditional::Conflict => panic!("unexpected ETag conflict on update"),
        };

        // Clean up.
        match backend.delete_task(&created.href, &new_etag).await? {
            Conditional::Ok(()) => {}
            Conditional::Conflict => panic!("unexpected ETag conflict on delete"),
        }
        assert!(
            !backend.list_tasks().await?.iter().any(|t| t.uid == uid),
            "task should be gone after delete"
        );
        eprintln!("live round-trip OK (create → list → update → delete)");
        Ok(())
    }
}
