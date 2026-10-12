import EventKit
import SwiftUI

/// Where to-dos with reminders go on this device: any reminders list or
/// calendar from the accounts in iOS Settings, or nowhere.
struct ReminderSettingsView: View {
    @Environment(AppModel.self) private var model

    @State private var destination = ReminderDestination.saved
    @State private var access: [ReminderDestination.Kind: Bool] = [:]

    var body: some View {
        Form {
            Section {
                row(title: "Don't sync", subtitle: nil, selected: destination == nil) {
                    choose(nil)
                }
            } footer: {
                Text("Reminders stay in mnemis only.")
            }
            section(.reminders, header: "Reminders lists",
                    footer: "Due at the time, with an alert. Ticking one off marks the to-do done.")
            section(.calendar, header: "Calendars",
                    footer: "An all-day event on the day, with an alert at the time.")
        }
        .navigationTitle("Reminders")
        .navigationBarTitleDisplayMode(.inline)
        .onAppear(perform: refreshAccess)
    }

    @ViewBuilder
    private func section(_ kind: ReminderDestination.Kind, header: String, footer: String) -> some View {
        Section {
            if access[kind] == true {
                let calendars = model.eventStore.calendars(for: kind.entityType)
                    .filter(\.allowsContentModifications)
                    .sorted { ($0.source.title, $0.title) < ($1.source.title, $1.title) }
                if calendars.isEmpty {
                    Text("None on this device").foregroundStyle(.secondary)
                }
                ForEach(calendars, id: \.calendarIdentifier) { calendar in
                    row(
                        title: calendar.title,
                        subtitle: calendar.source.title,
                        color: Color(cgColor: calendar.cgColor),
                        selected: destination == ReminderDestination(kind: kind, calendarId: calendar.calendarIdentifier)
                    ) {
                        choose(ReminderDestination(kind: kind, calendarId: calendar.calendarIdentifier))
                    }
                }
            } else {
                Button("Allow access") { Task { await requestAccess(kind) } }
            }
        } header: {
            Text(header)
        } footer: {
            Text(footer)
        }
    }

    private func row(
        title: String, subtitle: String?, color: Color? = nil, selected: Bool,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack {
                if let color {
                    Circle().fill(color).frame(width: 10, height: 10)
                }
                VStack(alignment: .leading) {
                    Text(title).foregroundStyle(.primary)
                    if let subtitle {
                        Text(subtitle).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if selected {
                    Image(systemName: "checkmark").foregroundStyle(.tint)
                }
            }
        }
    }

    private func choose(_ new: ReminderDestination?) {
        destination = new
        ReminderDestination.saved = new
        model.configureReminders()
        Task { await model.syncReminders() }
    }

    private func refreshAccess() {
        access[.reminders] = ReminderDestination.hasAccess(.reminders)
        access[.calendar] = ReminderDestination.hasAccess(.calendar)
    }

    private func requestAccess(_ kind: ReminderDestination.Kind) async {
        do {
            switch kind {
            case .reminders: _ = try await model.eventStore.requestFullAccessToReminders()
            case .calendar: _ = try await model.eventStore.requestFullAccessToEvents()
            }
        } catch {
            model.report(error)
        }
        refreshAccess()
    }
}
