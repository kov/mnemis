//! iCalendar VEVENT (de)serialization for the CalDAV backend.
//!
//! A mnemis action with a due date becomes an **all-day** VEVENT on that date,
//! carrying a DISPLAY VALARM that fires at 09:00 the morning of (9h after the
//! all-day start, which is midnight). iCloud syncs VEVENTs over CalDAV to the
//! Calendar app — unlike VTODO reminders, which it dropped (see the
//! `icloud-reminders-no-caldav` memory). The all-day date is taken/emitted in
//! UTC; v1 doesn't resolve per-event timezones.

use chrono::{DateTime, Duration, Utc};
use icalendar::{
    Alarm, Calendar, CalendarDateTime, Component, DatePerhapsTime, Event, EventLike, Trigger,
};

use super::TaskWrite;

/// 09:00 on the morning of the all-day due date: the all-day VEVENT starts at
/// midnight, so the alarm triggers 9 hours after start.
const ALARM_AFTER_START_HOURS: i64 = 9;

/// The fields we read back out of a server VEVENT. `uid` is the join key against
/// `actions.external_calendar_uid`; everything else is best-effort (a malformed
/// or partial event yields `None` fields rather than failing the whole sync).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEvent {
    pub uid: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub due: Option<i64>,
}

/// Build a single-VEVENT `VCALENDAR` body: an all-day event on the due date with
/// a morning-of alarm. `Calendar::new()` prefills the `VERSION`/`PRODID`/
/// `CALSCALE` headers servers require.
pub fn task_to_ics(task: &TaskWrite) -> String {
    let mut event = Event::new();
    event.uid(&task.uid);
    event.summary(&task.summary);
    if let Some(desc) = &task.description {
        event.description(desc);
    }
    if let Some(date) = task
        .due
        .and_then(|d| DateTime::<Utc>::from_timestamp(d, 0))
        .map(|dt| dt.date_naive())
    {
        // All-day: DTSTART = the day, DTEND = the next day (exclusive).
        event.starts(date);
        event.ends(date.succ_opt().unwrap_or(date));
        // A morning-of nudge. The calendar event *is* the reminder; any extra
        // mnemis-side notification is a separate layer on top (v2 Phase 5).
        event.alarm(Alarm::display(
            &task.summary,
            Trigger::after_start(Duration::hours(ALARM_AFTER_START_HOURS)),
        ));
    }
    Calendar::new().push(event.done()).done().to_string()
}

/// Parse the first VEVENT out of a `calendar-data` payload. Returns `None` only
/// when the body has no VEVENT at all; an event with missing properties still
/// parses (with `None` fields) so a quirky server entry doesn't abort the run.
pub fn parse_event(ics: &str) -> Option<ParsedEvent> {
    let calendar: Calendar = ics.parse().ok()?;
    let event = calendar.components.iter().find_map(|c| c.as_event())?;
    Some(ParsedEvent {
        uid: event.get_uid().map(str::to_owned),
        summary: event.get_summary().map(str::to_owned),
        description: event.get_description().map(str::to_owned),
        due: event
            .get_start()
            .and_then(|d| date_perhaps_time_to_unix(&d)),
    })
}

/// Normalise an iCalendar date/date-time to a unix timestamp. All-day (`DATE`)
/// and floating values are treated as UTC; a `WithTimezone` value is read as its
/// naive wall-clock in UTC (we don't resolve `VTIMEZONE` in v1).
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

    fn write(due: Option<i64>) -> TaskWrite {
        TaskWrite {
            uid: "mnemis-action-7@mnemis".into(),
            summary: "Reply to Ana about the budget".into(),
            description: Some("She asked twice; deadline Friday.".into()),
            due,
        }
    }

    #[test]
    fn round_trips_core_fields_as_all_day_event() {
        // 2026-06-01T00:00:00Z — a date-only due.
        let due = 1_780_272_000;
        let ics = task_to_ics(&write(Some(due)));
        // A valid VCALENDAR with the required headers and an all-day VEVENT.
        assert!(ics.contains("BEGIN:VCALENDAR"));
        assert!(ics.contains("VERSION:2.0"));
        assert!(ics.contains("BEGIN:VEVENT"));
        assert!(
            ics.contains("DTSTART;VALUE=DATE:20260601"),
            "all-day DTSTART:\n{ics}"
        );
        assert!(
            ics.contains("DTEND;VALUE=DATE:20260602"),
            "exclusive next-day DTEND:\n{ics}"
        );

        let parsed = parse_event(&ics).expect("a VEVENT");
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
    }

    #[test]
    fn always_carries_a_morning_of_alarm() {
        let ics = task_to_ics(&write(Some(1_780_272_000)));
        assert!(ics.contains("BEGIN:VALARM"), "a VALARM is present:\n{ics}");
        assert!(ics.contains("ACTION:DISPLAY"));
        // 9h after the midnight all-day start = 09:00 the morning of. icalendar
        // serialises the duration in seconds (9h = 32400s), related to START.
        assert!(
            ics.contains("TRIGGER;RELATED=START:PT32400S"),
            "morning-of trigger:\n{ics}"
        );
    }

    #[test]
    fn accepts_all_day_date_from_server() {
        // What iCloud stores for an all-day event: DTSTART as a bare DATE.
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//Apple Inc.//Mac OS X//EN\r
BEGIN:VEVENT\r
UID:abc-123\r
SUMMARY:Pay rent\r
DTSTART;VALUE=DATE:20260601\r
DTEND;VALUE=DATE:20260602\r
END:VEVENT\r
END:VCALENDAR\r
";
        let parsed = parse_event(ics).expect("a VEVENT");
        assert_eq!(parsed.uid.as_deref(), Some("abc-123"));
        assert_eq!(parsed.summary.as_deref(), Some("Pay rent"));
        // 2026-06-01T00:00:00Z
        assert_eq!(parsed.due, Some(1_780_272_000));
    }

    #[test]
    fn no_vevent_yields_none() {
        let ics = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//x//y//EN\r
BEGIN:VTODO\r
UID:t1\r
SUMMARY:A task\r
END:VTODO\r
END:VCALENDAR\r
";
        assert_eq!(parse_event(ics), None);
    }
}
