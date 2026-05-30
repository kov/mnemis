//! iCalendar VTODO (de)serialization for the CalDAV backend.
//!
//! This is the CalDAV *wire format* layer: it turns a provider-agnostic
//! [`TaskWrite`] into a `VCALENDAR`/`VTODO` body for `PUT`, and parses a
//! `calendar-data` payload from a `REPORT` back into a [`ParsedVtodo`]. Kept
//! separate from the [`TaskBackend`](super::TaskBackend) seam so a non-CalDAV
//! backend (e.g. Google Tasks) never touches iCalendar.
//!
//! Times round-trip as unix seconds. We emit `DUE` as a UTC `DATE-TIME`
//! (`…Z`); on the way back we accept whatever the server stored — UTC,
//! floating, or a bare `DATE` (all-day reminders) — and normalise to a unix
//! timestamp, treating floating/all-day values as UTC midnight (best effort,
//! since VTIMEZONE resolution is out of scope for v1).

use chrono::{DateTime, Utc};
use icalendar::{Calendar, CalendarDateTime, Component, DatePerhapsTime, Todo, TodoStatus};

use super::{TaskStatus, TaskWrite};

impl TaskStatus {
    fn to_todo(self) -> TodoStatus {
        match self {
            Self::NeedsAction => TodoStatus::NeedsAction,
            Self::InProcess => TodoStatus::InProcess,
            Self::Completed => TodoStatus::Completed,
            Self::Cancelled => TodoStatus::Cancelled,
        }
    }

    fn from_todo(status: TodoStatus) -> Self {
        match status {
            TodoStatus::NeedsAction => Self::NeedsAction,
            TodoStatus::InProcess => Self::InProcess,
            TodoStatus::Completed => Self::Completed,
            TodoStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// The fields we read back out of a server VTODO. `uid` is the join key against
/// `actions.external_calendar_uid`; everything else is best-effort (a malformed
/// or partial VTODO yields `None` fields rather than failing the whole sync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedVtodo {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub due: Option<i64>,
    pub status: Option<TaskStatus>,
}

/// Build a single-VTODO `VCALENDAR` body. `Calendar::new()` prefills the
/// `VERSION`/`PRODID`/`CALSCALE` headers servers require.
pub fn task_to_ics(task: &TaskWrite) -> String {
    let mut todo = Todo::new();
    todo.uid(&task.uid);
    todo.summary(&task.summary);
    if let Some(desc) = &task.description {
        todo.description(desc);
    }
    if let Some(dt) = task
        .due
        .and_then(|due| DateTime::<Utc>::from_timestamp(due, 0))
    {
        todo.due(dt);
    }
    todo.status(task.status.to_todo());
    if task.status == TaskStatus::Completed {
        // Apple Reminders shows the checkmark off PERCENT-COMPLETE as well as
        // STATUS; set both so a pushed completion renders as done.
        todo.percent_complete(100);
    }
    Calendar::new().push(todo.done()).done().to_string()
}

/// Parse the first VTODO out of a `calendar-data` payload. Returns `None` only
/// when the body has no VTODO at all; a VTODO with missing properties still
/// parses (with `None` fields) so a quirky server entry doesn't abort the run.
pub fn parse_vtodo(ics: &str) -> Option<ParsedVtodo> {
    let calendar: Calendar = ics.parse().ok()?;
    let todo = calendar.components.iter().find_map(|c| c.as_todo())?;
    Some(ParsedVtodo {
        uid: todo.get_uid().map(str::to_owned),
        summary: todo.get_summary().map(str::to_owned),
        description: todo.get_description().map(str::to_owned),
        due: todo.get_due().and_then(|d| date_perhaps_time_to_unix(&d)),
        status: todo.get_status().map(TaskStatus::from_todo),
    })
}

/// Normalise an iCalendar date/date-time to a unix timestamp. All-day (`DATE`)
/// and floating values are treated as UTC; a `WithTimezone` value is read as
/// its naive wall-clock in UTC (we don't resolve `VTIMEZONE` in v1).
fn date_perhaps_time_to_unix(dpt: &DatePerhapsTime) -> Option<i64> {
    match dpt {
        DatePerhapsTime::Date(date) => Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp()),
        DatePerhapsTime::DateTime(cdt) => match cdt {
            CalendarDateTime::Utc(dt) => Some(dt.timestamp()),
            CalendarDateTime::Floating(naive)
            | CalendarDateTime::WithTimezone {
                date_time: naive, ..
            } => Some(naive.and_utc().timestamp()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(status: TaskStatus, due: Option<i64>) -> TaskWrite {
        TaskWrite {
            uid: "mnemis-action-7@mnemis".into(),
            summary: "Reply to Ana about the budget".into(),
            description: Some("She asked twice; deadline Friday.".into()),
            due,
            status,
        }
    }

    #[test]
    fn round_trips_core_fields_through_ics() {
        // 2026-06-01T09:00:00Z
        let due = 1_780_304_400;
        let ics = task_to_ics(&write(TaskStatus::NeedsAction, Some(due)));
        // It is a valid VCALENDAR with the required headers.
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("VERSION:2.0"));
        assert!(ics.contains("BEGIN:VTODO"));

        let parsed = parse_vtodo(&ics).expect("a VTODO");
        assert_eq!(parsed.uid.as_deref(), Some("mnemis-action-7@mnemis"));
        assert_eq!(
            parsed.summary.as_deref(),
            Some("Reply to Ana about the budget")
        );
        assert_eq!(
            parsed.description.as_deref(),
            Some("She asked twice; deadline Friday.")
        );
        assert_eq!(parsed.due, Some(due));
        assert_eq!(parsed.status, Some(TaskStatus::NeedsAction));
    }

    #[test]
    fn status_maps_both_directions() {
        for status in [
            TaskStatus::NeedsAction,
            TaskStatus::InProcess,
            TaskStatus::Completed,
            TaskStatus::Cancelled,
        ] {
            let ics = task_to_ics(&write(status, None));
            let parsed = parse_vtodo(&ics).expect("a VTODO");
            assert_eq!(parsed.status, Some(status), "round-trip for {status:?}");
        }
    }

    #[test]
    fn no_due_is_omitted_not_zero() {
        let ics = task_to_ics(&write(TaskStatus::NeedsAction, None));
        assert!(!ics.contains("DUE"), "no DUE line when due is None:\n{ics}");
        assert_eq!(parse_vtodo(&ics).unwrap().due, None);
    }

    #[test]
    fn completed_sets_percent_complete() {
        let ics = task_to_ics(&write(TaskStatus::Completed, None));
        assert!(ics.contains("PERCENT-COMPLETE:100"));
    }

    #[test]
    fn accepts_all_day_date_due_from_server() {
        // A server (e.g. Apple Reminders for an all-day task) may store DUE as a
        // bare DATE. We must still surface a timestamp.
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//Apple Inc.//Reminders//EN\r
BEGIN:VTODO\r
UID:abc-123\r
SUMMARY:Pay rent\r
DUE;VALUE=DATE:20260601\r
STATUS:NEEDS-ACTION\r
END:VTODO\r
END:VCALENDAR\r
";
        let parsed = parse_vtodo(ics).expect("a VTODO");
        assert_eq!(parsed.uid.as_deref(), Some("abc-123"));
        assert_eq!(parsed.summary.as_deref(), Some("Pay rent"));
        // 2026-06-01T00:00:00Z
        assert_eq!(parsed.due, Some(1_780_272_000));
    }

    #[test]
    fn no_vtodo_yields_none() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//x//y//EN\r
BEGIN:VEVENT\r
UID:e1\r
SUMMARY:A meeting\r
END:VEVENT\r
END:VCALENDAR\r
";
        assert_eq!(parse_vtodo(ics), None);
    }
}
