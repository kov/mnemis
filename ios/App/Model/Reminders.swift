import EventKit
import Foundation

/// A reminder's day and time as the engine keeps them: the day as midnight
/// UTC of the date (`ActionDto.dueAt`), the time as minutes after local
/// midnight (`remindMinutes`, 09:00 when unset).
enum ReminderTime {
    static let defaultMinutes: Int64 = 9 * 60

    private static var utc: Calendar {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC")!
        return calendar
    }

    /// The date of a stored day.
    static func components(ofDay day: Int64) -> DateComponents {
        utc.dateComponents([.year, .month, .day], from: Date(timeIntervalSince1970: TimeInterval(day)))
    }

    /// The stored day of a date's year, month and day.
    static func day(of components: DateComponents) -> Int64? {
        var date = DateComponents()
        date.year = components.year
        date.month = components.month
        date.day = components.day
        return utc.date(from: date).map { Int64($0.timeIntervalSince1970) }
    }

    /// The stored day of a local date.
    static func day(ofLocal date: Date) -> Int64 {
        day(of: Calendar.current.dateComponents([.year, .month, .day], from: date))!
    }

    /// The local moment a reminder alerts.
    static func alert(day: Int64, minutes: Int64?) -> Date {
        var parts = components(ofDay: day)
        let minutes = minutes ?? defaultMinutes
        parts.hour = Int(minutes / 60)
        parts.minute = Int(minutes % 60)
        return Calendar.current.date(from: parts) ?? Date(timeIntervalSince1970: TimeInterval(day))
    }

    /// Minutes after local midnight of a moment.
    static func minutes(of date: Date) -> Int64 {
        let parts = Calendar.current.dateComponents([.hour, .minute], from: date)
        return Int64((parts.hour ?? 0) * 60 + (parts.minute ?? 0))
    }
}

/// Where this device keeps mnemis's reminders: a reminders list or a
/// calendar, from any account in iOS Settings. Saved per device, as
/// EventKit's identifiers are.
struct ReminderDestination: Equatable {
    enum Kind: String {
        case reminders
        case calendar

        var entityType: EKEntityType { self == .reminders ? .reminder : .event }
    }

    var kind: Kind
    var calendarId: String

    private static let kindKey = "reminderDestinationKind"
    private static let calendarKey = "reminderDestinationCalendar"

    static var saved: ReminderDestination? {
        get {
            let defaults = UserDefaults.standard
            guard let raw = defaults.string(forKey: kindKey), let kind = Kind(rawValue: raw),
                  let id = defaults.string(forKey: calendarKey) else { return nil }
            return ReminderDestination(kind: kind, calendarId: id)
        }
        set {
            let defaults = UserDefaults.standard
            defaults.set(newValue?.kind.rawValue, forKey: kindKey)
            defaults.set(newValue?.calendarId, forKey: calendarKey)
        }
    }

    static func hasAccess(_ kind: Kind) -> Bool {
        EKEventStore.authorizationStatus(for: kind.entityType) == .fullAccess
    }
}

/// mnemis's reminders in an EventKit reminders list or calendar. Each item
/// links back to its to-do (`mnemis://action/<id>?uid=<uid>`), which is also
/// how mnemis tells its items from the user's own. A list item is a
/// reminder due on the day at the time, with an alert then; a calendar item
/// is an all-day event with an alert at the time.
final class EventKitReminders: ReminderStore, @unchecked Sendable {
    private let store: EKEventStore
    private let destination: ReminderDestination

    init(store: EKEventStore, destination: ReminderDestination) {
        self.store = store
        self.destination = destination
    }

    private func calendar() throws -> EKCalendar {
        guard let calendar = store.calendar(withIdentifier: destination.calendarId) else {
            throw MnemisError.Failed(message: "The reminders list or calendar chosen in Settings is gone.")
        }
        return calendar
    }

    private static func etag(_ item: EKCalendarItem) -> String {
        String(item.lastModifiedDate?.timeIntervalSince1970 ?? 0)
    }

    private static func uid(_ item: EKCalendarItem) -> String? {
        guard let url = item.url, url.scheme == "mnemis", url.host() == "action" else { return nil }
        return URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first { $0.name == "uid" }?.value
    }

    private static func link(_ item: ReminderWrite) -> URL? {
        var parts = URLComponents()
        parts.scheme = "mnemis"
        parts.host = "action"
        parts.path = "/\(item.actionId)"
        parts.queryItems = [URLQueryItem(name: "uid", value: item.uid)]
        return parts.url
    }

    func list() async throws -> [StoredReminder] {
        let calendar = try calendar()
        switch destination.kind {
        case .reminders:
            let predicate = store.predicateForReminders(in: [calendar])
            let reminders: [EKReminder] = await withCheckedContinuation { done in
                store.fetchReminders(matching: predicate) { done.resume(returning: $0 ?? []) }
            }
            return reminders.compactMap { reminder in
                guard let uid = Self.uid(reminder) else { return nil }
                let due = reminder.dueDateComponents
                return StoredReminder(
                    uid: uid,
                    href: reminder.calendarItemIdentifier,
                    etag: Self.etag(reminder),
                    title: reminder.title ?? "",
                    notes: reminder.notes,
                    due: due.flatMap(ReminderTime.day(of:)),
                    alertMinutes: due?.hour.map { UInt32($0 * 60 + (due?.minute ?? 0)) },
                    completed: reminder.isCompleted
                )
            }
        case .calendar:
            let now = Date()
            let predicate = store.predicateForEvents(
                withStart: now.addingTimeInterval(-400 * 86_400),
                end: now.addingTimeInterval(3 * 365 * 86_400),
                calendars: [calendar]
            )
            return store.events(matching: predicate).compactMap { event in
                guard let uid = Self.uid(event) else { return nil }
                let offset = event.alarms?.first?.relativeOffset
                return StoredReminder(
                    uid: uid,
                    href: event.calendarItemIdentifier,
                    etag: Self.etag(event),
                    title: event.title ?? "",
                    notes: event.notes,
                    due: ReminderTime.day(ofLocal: event.startDate),
                    alertMinutes: offset.flatMap { $0 >= 0 && $0 < 86_400 ? UInt32($0 / 60) : nil },
                    completed: false
                )
            }
        }
    }

    func create(item: ReminderWrite) async throws -> ReminderCreated {
        let calendar = try calendar()
        let created: EKCalendarItem
        switch destination.kind {
        case .reminders:
            let reminder = EKReminder(eventStore: store)
            reminder.calendar = calendar
            fill(reminder, item)
            try store.save(reminder, commit: true)
            created = reminder
        case .calendar:
            let event = EKEvent(eventStore: store)
            event.calendar = calendar
            fill(event, item)
            try store.save(event, span: .thisEvent, commit: true)
            created = event
        }
        return ReminderCreated(href: created.calendarItemIdentifier, etag: Self.etag(created))
    }

    func update(href: String, etag: String, item: ReminderWrite) async throws -> String? {
        guard let current = store.calendarItem(withIdentifier: href), Self.etag(current) == etag else {
            return nil
        }
        if let reminder = current as? EKReminder {
            fill(reminder, item)
            try store.save(reminder, commit: true)
        } else if let event = current as? EKEvent {
            fill(event, item)
            try store.save(event, span: .thisEvent, commit: true)
        }
        return Self.etag(current)
    }

    func delete(href: String, etag: String) async throws -> Bool {
        // Already gone counts as deleted.
        guard let current = store.calendarItem(withIdentifier: href) else { return true }
        guard Self.etag(current) == etag else { return false }
        if let reminder = current as? EKReminder {
            try store.remove(reminder, commit: true)
        } else if let event = current as? EKEvent {
            try store.remove(event, span: .thisEvent, commit: true)
        }
        return true
    }

    func complete(href: String, etag: String) async throws -> Bool {
        guard let reminder = store.calendarItem(withIdentifier: href) as? EKReminder else {
            // A calendar has no "done": the event goes.
            return try await delete(href: href, etag: etag)
        }
        guard Self.etag(reminder) == etag else { return false }
        reminder.isCompleted = true
        try store.save(reminder, commit: true)
        return true
    }

    private func fill(_ reminder: EKReminder, _ item: ReminderWrite) {
        reminder.title = item.title
        reminder.notes = item.notes
        reminder.url = Self.link(item)
        if let day = item.due {
            let alert = ReminderTime.alert(day: day, minutes: item.alertMinutes.map(Int64.init))
            var due = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute], from: alert)
            due.timeZone = .current
            reminder.dueDateComponents = due
            reminder.alarms = [EKAlarm(absoluteDate: alert)]
        } else {
            reminder.dueDateComponents = nil
            reminder.alarms = nil
        }
    }

    private func fill(_ event: EKEvent, _ item: ReminderWrite) {
        event.title = item.title
        event.notes = item.notes
        event.url = Self.link(item)
        let day = item.due ?? ReminderTime.day(ofLocal: Date())
        let start = ReminderTime.alert(day: day, minutes: 0)
        event.isAllDay = true
        event.startDate = start
        event.endDate = start
        let minutes = item.alertMinutes.map(Int64.init) ?? ReminderTime.defaultMinutes
        event.alarms = [EKAlarm(relativeOffset: TimeInterval(minutes * 60))]
    }
}
