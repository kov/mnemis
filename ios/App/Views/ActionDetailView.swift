import SwiftUI

/// A to-do, kept current as it changes (a reminder set, a sync).
struct ActionDetailView: View {
    @Environment(AppModel.self) private var model
    let actionId: Int64

    var body: some View {
        if let action = model.action(actionId) {
            ActionDetailBody(action: action)
        } else {
            ContentUnavailableView("No longer here", systemImage: "checklist")
        }
    }
}

private struct ActionDetailBody: View {
    @Environment(AppModel.self) private var model
    let action: ActionDto
    @State private var message: MessageDetailDto?
    @State private var settingReminder = false

    private var isWarning: Bool { action.status == .suspicious }
    private var isResolved: Bool { [.done, .cancelled, .dismissed].contains(action.status) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Card {
                    if isWarning {
                        Label("Looks like phishing", systemImage: "exclamationmark.triangle.fill")
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(.orange)
                    } else {
                        HStack(spacing: 6) {
                            ConfidenceBadge(confidence: action.confidence)
                            if let due = action.dueAt {
                                DueBadge(due: due, minutes: action.remindMinutes, showsTime: true)
                            }
                        }
                    }
                    Text(action.title)
                        .font(.title2.weight(.bold))
                        .fixedSize(horizontal: false, vertical: true)
                    if let details = action.details, !details.isEmpty {
                        Text(details)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }

                if isWarning {
                    GlassEffectContainer(spacing: 10) {
                        HStack(spacing: 10) {
                            acceptButton
                            discardButton
                        }
                        .controlSize(.large)
                        .labelStyle(.titleAndIcon)
                        .font(.subheadline.weight(.semibold))
                    }
                } else if isResolved {
                    GlassEffectContainer(spacing: 10) {
                        HStack(spacing: 10) {
                            reopenButton
                            talkButton
                        }
                        .controlSize(.large)
                        .labelStyle(.titleAndIcon)
                        .font(.subheadline.weight(.semibold))
                    }
                } else {
                    GlassEffectContainer(spacing: 10) {
                        // One row when it fits; otherwise Done and Dismiss share a
                        // row and the reminder and "Talk about this" the next.
                        ViewThatFits(in: .horizontal) {
                            HStack(spacing: 10) {
                                doneButton
                                reminderButton
                                talkButton
                                dismissButton
                            }
                            VStack(spacing: 10) {
                                HStack(spacing: 10) {
                                    doneButton
                                    dismissButton
                                }
                                HStack(spacing: 10) {
                                    reminderButton
                                    talkButton
                                }
                            }
                        }
                        .controlSize(.large)
                        .labelStyle(.titleAndIcon)
                        .font(.subheadline.weight(.semibold))
                    }
                }

                if let threadId = action.sourceThreadId {
                    // The email as the reader shows it, with its controls.
                    ThreadContent(threadId: threadId, viewing: action.id)
                } else if let message {
                    Card {
                        Label("Original email", systemImage: "envelope")
                            .font(.footnote.weight(.semibold))
                            .foregroundStyle(.secondary)
                        // No replying to an impostor from here.
                        if !isWarning {
                            replyButtons(message)
                        }
                        if let subject = message.subject {
                            Text(subject).font(.headline)
                        }
                        HStack(alignment: .firstTextBaseline) {
                            if let author = message.authorDisplay ?? message.authorAddr {
                                Text(author).font(.subheadline.weight(.medium))
                            }
                            Spacer()
                            Text(Date.fromUnix(message.postedAt), format: .dateTime.day().month().hour().minute())
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Divider()
                        Text(message.body)
                            .font(.callout)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            .padding()
            .frame(maxWidth: 760)
            .frame(maxWidth: .infinity)
        }
        .background(Color(.systemGroupedBackground))
        .navigationTitle(isWarning ? "Suspicious email" : "To-do")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: action.id) { await loadMessage() }
        .sheet(isPresented: $settingReminder) {
            ReminderSheet(action: action)
        }
    }

    private var reminderButton: some View {
        Button {
            settingReminder = true
        } label: {
            Label(action.dueAt == nil ? "Set reminder" : "Change reminder", systemImage: "bell")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
    }

    private var doneButton: some View {
        Button {
            Task { await model.setStatus(action, to: .done) }
        } label: {
            Label("Done", systemImage: "checkmark")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glassProminent)
    }

    private var acceptButton: some View {
        Button {
            Task { await model.accept(action) }
        } label: {
            Label("It's a to-do", systemImage: "checklist")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
    }

    private var discardButton: some View {
        Button {
            Task { await model.discard(action) }
        } label: {
            Label("Dismiss", systemImage: "xmark")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glassProminent)
    }

    private var talkButton: some View {
        Button {
            Task { await model.talkAbout(action) }
        } label: {
            Label("Talk about this", systemImage: "bubble.left.and.text.bubble.right")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
    }

    private var reopenButton: some View {
        Button {
            Task { await model.setStatus(action, to: .pending) }
        } label: {
            Label("Bring back", systemImage: "arrow.uturn.backward")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glassProminent)
    }

    private var dismissButton: some View {
        Button {
            Task { await model.dismiss(action) }
        } label: {
            Label("Dismiss", systemImage: "xmark")
                .lineLimit(1)
                .fixedSize()
                .frame(maxWidth: .infinity)
        }
        .buttonStyle(.glass)
        .tint(.red)
    }

    /// Reply, Reply all and Forward, labelled when they fit, icons otherwise.
    private func replyButtons(_ message: MessageDetailDto) -> some View {
        GlassEffectContainer(spacing: 8) {
            ViewThatFits(in: .horizontal) {
                replyRow(message).labelStyle(.titleAndIcon)
                replyRow(message).labelStyle(.iconOnly)
            }
            .buttonStyle(.glass)
            .font(.subheadline.weight(.semibold))
        }
    }

    private func replyRow(_ message: MessageDetailDto) -> some View {
        HStack(spacing: 8) {
            Button("Reply", systemImage: "arrowshape.turn.up.left") {
                Task { await model.compose(.reply, messageId: message.id) }
            }
            Button("Reply all", systemImage: "arrowshape.turn.up.left.2") {
                Task { await model.compose(.replyAll, messageId: message.id) }
            }
            Button("Forward", systemImage: "arrowshape.turn.up.right") {
                Task { await model.compose(.forward, messageId: message.id) }
            }
        }
        .lineLimit(1)
        .fixedSize()
    }

    private func loadMessage() async {
        message = nil
        guard action.sourceThreadId == nil, let engine = model.engine,
              let id = action.sourceMessageId else { return }
        do {
            message = try await engine.getMessage(messageId: id)
            if !isWarning {
                // So Reply opens without waiting on the server.
                Task { try? await engine.prefetchOriginal(messageId: id) }
            }
        } catch {
            model.report(error)
        }
    }
}

/// Set a to-do's reminder, or move or remove the one it has.
struct ReminderSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let action: ActionDto

    @State private var when: Date

    init(action: ActionDto) {
        self.action = action
        let day = action.dueAt ?? ReminderTime.day(ofLocal: Date().addingTimeInterval(86_400))
        _when = State(initialValue: ReminderTime.alert(day: day, minutes: action.remindMinutes))
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    DatePicker("Day", selection: $when, displayedComponents: .date)
                        .datePickerStyle(.graphical)
                    DatePicker("Alert", selection: $when, displayedComponents: .hourAndMinute)
                } footer: {
                    if ReminderDestination.saved == nil {
                        Text("Choose a reminders list or calendar in Settings to get it there too.")
                    }
                }
                Section {
                    // Three hours from now, on the half hour, while it's still today.
                    if let later = laterToday {
                        quick("Later today", systemImage: "clock", at: later)
                    }
                    quick("Tomorrow", systemImage: "sunrise", at: shifted(days: 1))
                    quick("Next week", systemImage: "calendar", at: shifted(days: 7))
                }
                if action.dueAt != nil {
                    Section {
                        Button("Remove reminder", systemImage: "bell.slash", role: .destructive) {
                            Task {
                                await model.clearReminder(action)
                                dismiss()
                            }
                        }
                    }
                }
            }
            .navigationTitle(action.dueAt == nil ? "Set reminder" : "Change reminder")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") { save(when) }
                }
            }
        }
    }

    private var laterToday: Date? {
        let minutes = (ReminderTime.minutes(of: Date()) + 180 + 29) / 30 * 30
        guard minutes < 24 * 60 else { return nil }
        return ReminderTime.alert(day: ReminderTime.day(ofLocal: Date()), minutes: minutes)
    }

    /// `days` from today, at the time chosen.
    private func shifted(days: Int) -> Date {
        let day = Calendar.current.date(byAdding: .day, value: days, to: Date()) ?? Date()
        return ReminderTime.alert(day: ReminderTime.day(ofLocal: day), minutes: ReminderTime.minutes(of: when))
    }

    private func quick(_ title: String, systemImage: String, at date: Date) -> some View {
        Button {
            save(date)
        } label: {
            HStack {
                Label(title, systemImage: systemImage)
                Spacer()
                Text(date, format: .dateTime.weekday().day().month().hour().minute())
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func save(_ date: Date) {
        Task {
            await model.setReminder(action, day: ReminderTime.day(ofLocal: date), minutes: ReminderTime.minutes(of: date))
            dismiss()
        }
    }
}

/// To-dos done, cancelled or dismissed in the last 30 days, most recent
/// first, to bring one back.
struct ResolvedActionsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List(model.resolved, id: \.id) { action in
            NavigationLink {
                ActionDetailView(actionId: action.id)
            } label: {
                VStack(alignment: .leading, spacing: 6) {
                    Text(action.title)
                        .font(.body.weight(.medium))
                        .lineLimit(2)
                    Text(Self.label(action.status))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
            }
            .swipeActions(edge: .leading) {
                Button("Bring back", systemImage: "arrow.uturn.backward") {
                    Task { await model.setStatus(action, to: .pending) }
                }
                .tint(.blue)
            }
        }
        .overlay {
            if model.resolved.isEmpty {
                ContentUnavailableView(
                    "Nothing done lately",
                    systemImage: "clock.arrow.circlepath",
                    description: Text("To-dos you finish or dismiss show here for 30 days.")
                )
            }
        }
        .navigationTitle("Recently done")
        .navigationBarTitleDisplayMode(.inline)
        .task { await model.refresh() }
    }

    private static func label(_ status: ActionStatus) -> String {
        switch status {
        case .done: "Done"
        case .cancelled: "Cancelled"
        case .dismissed: "Dismissed"
        default: ""
        }
    }
}
