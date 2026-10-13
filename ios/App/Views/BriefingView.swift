import SwiftUI

/// The periods offered; anything else is a custom one.
private let presetDays: [Int64] = [1, 3, 7]

/// The landing screen: what mail came in over the last few days and what of
/// it to read. Everything but the written summary comes from tags stored as
/// mail syncs, so switching periods is instant.
struct BriefingStack: View {
    var body: some View {
        NavigationStack {
            BriefingView()
        }
    }
}

struct BriefingView: View {
    @Environment(AppModel.self) private var model
    @State private var briefing: BriefingDto?
    @State private var choosingDays = false

    var body: some View {
        @Bindable var model = model
        List {
            if let briefing {
                SummarySection(briefing: briefing)
                if !briefing.worthReading.isEmpty {
                    Section("Worth reading") {
                        ForEach(briefing.worthReading, id: \.messageId) { MailLink(mail: $0) }
                    }
                }
                if !briefing.watches.isEmpty {
                    Section("Watching for") {
                        ForEach(briefing.watches, id: \.watchId) { WatchRow(watch: $0) }
                    }
                }
                if !briefing.newTodos.isEmpty || briefing.resolvedTodos > 0 {
                    TodosSection(briefing: briefing)
                }
                if !briefing.suspicious.isEmpty {
                    Section("Suspicious") {
                        ForEach(briefing.suspicious, id: \.messageId) { MailLink(mail: $0) }
                    }
                }
                OverviewSection(briefing: briefing)
            }
        }
        .overlay {
            if briefing == nil {
                ProgressView()
            }
        }
        .navigationTitle("Briefing")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                PeriodMenu(days: $model.briefingDays, choosingCustom: $choosingDays)
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button("Ask about this", systemImage: "bubble.left.and.text.bubble.right") {
                    Task { await model.askAboutBriefing(days: model.briefingDays) }
                }
                .disabled(model.engine == nil)
            }
        }
        .refreshable { await model.sync() }
        .task(id: "\(model.briefingDays)-\(model.mailVersion)-\(model.briefingVersion)") { await load() }
        .sheet(isPresented: $choosingDays) {
            CustomDaysSheet(days: $model.briefingDays)
        }
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            briefing = try await engine.briefing(days: model.briefingDays)
        } catch {
            model.report(error)
        }
    }
}

/// The header's period button: the presets, or a custom number of days.
private struct PeriodMenu: View {
    @Binding var days: Int64
    @Binding var choosingCustom: Bool

    var body: some View {
        Menu {
            Picker("Period", selection: $days) {
                ForEach(presetDays, id: \.self) { d in
                    Text(periodName(d)).tag(d)
                }
                if !presetDays.contains(days) {
                    Text(periodName(days)).tag(days)
                }
            }
            Button("Custom…", systemImage: "calendar") { choosingCustom = true }
        } label: {
            Label(periodName(days), systemImage: "calendar")
                .labelStyle(.titleAndIcon)
        }
    }
}

func periodName(_ days: Int64) -> String {
    days == 1 ? "1 day" : "\(days) days"
}

private struct CustomDaysSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var days: Int64
    @State private var choice: Int64 = 14

    var body: some View {
        NavigationStack {
            Form {
                Stepper("Last \(choice) days", value: $choice, in: 1...90)
            }
            .navigationTitle("Period")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        days = choice
                        dismiss()
                    }
                }
            }
        }
        .presentationDetents([.height(200)])
        .onAppear {
            if !presetDays.contains(days) { choice = days }
        }
    }
}

/// What the agent wrote about the period, and the emails it said to read.
private struct SummarySection: View {
    @Environment(AppModel.self) private var model
    let briefing: BriefingDto

    private var writing: Bool { model.writingBriefing.contains(briefing.days) }

    var body: some View {
        Section {
            if let written = briefing.written {
                Text(markdown(written.summary))
                    .font(.body)
                    .padding(.vertical, 4)
                ForEach(written.highlights, id: \.messageId) { MailLink(mail: $0) }
            } else if !writing {
                Text("No summary yet. mnemis can read through this period and tell you what matters.")
                    .foregroundStyle(.secondary)
            }
            if writing {
                Label("Reading your mail…", systemImage: "sparkles")
                    .foregroundStyle(.secondary)
                    .symbolEffect(.pulse)
            } else {
                Button(briefing.written == nil ? "Write a summary" : "Write it again", systemImage: "sparkles") {
                    Task { await model.writeBriefing(days: briefing.days) }
                }
                .disabled(model.engine == nil || briefing.received == 0)
            }
        } header: {
            Text("Summary")
        } footer: {
            if let written = briefing.written, !writing {
                let when = Date.fromUnix(written.generatedAt).formatted(.relative(presentation: .named))
                Text(written.newerMail > 0
                     ? "Written \(when). \(written.newerMail) new since."
                     : "Written \(when).")
            }
        }
    }

    private func markdown(_ text: String) -> AttributedString {
        (try? AttributedString(
            markdown: text,
            options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        )) ?? AttributedString(text)
    }
}

private struct OverviewSection: View {
    let briefing: BriefingDto

    var body: some View {
        Section {
            LabeledContent("Received", value: "\(briefing.received)")
            LabeledContent("Unread", value: "\(briefing.unread)")
            ForEach(briefing.categories, id: \.category) { c in
                LabeledContent(c.category.label, value: "\(c.count)")
                    .foregroundStyle(.secondary)
            }
            if briefing.untagged > 0 {
                LabeledContent("Not sorted yet", value: "\(briefing.untagged)")
                    .foregroundStyle(.secondary)
            }
            if briefing.accounts.count > 1 {
                ForEach(briefing.accounts, id: \.account) { a in
                    LabeledContent(a.account, value: "\(a.count)")
                        .foregroundStyle(.secondary)
                }
            }
        } header: {
            Text("Mail")
        }
    }
}

private struct TodosSection: View {
    let briefing: BriefingDto

    var body: some View {
        Section {
            ForEach(briefing.newTodos, id: \.actionId) { todo in
                NavigationLink {
                    ActionDetailView(actionId: todo.actionId)
                } label: {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(todo.title)
                            .lineLimit(2)
                        if let due = todo.dueAt {
                            DueBadge(due: due)
                        }
                    }
                }
            }
        } header: {
            Text("New to-dos")
        } footer: {
            if briefing.resolvedTodos > 0 {
                Text("\(briefing.resolvedTodos) done or dismissed in this period.")
            }
        }
    }
}

private struct WatchRow: View {
    let watch: BriefingWatchDto

    var body: some View {
        if let mail = watch.matched {
            VStack(alignment: .leading, spacing: 4) {
                Label(watch.lookingFor, systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green)
                MailLink(mail: mail)
            }
        } else {
            VStack(alignment: .leading, spacing: 4) {
                Label(watch.lookingFor, systemImage: "eye")
                if let until = watch.expiresAt {
                    Text("Until \(Date.fromUnix(until).formatted(date: .abbreviated, time: .omitted))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// An email with why it is listed; opens its thread.
private struct MailLink: View {
    let mail: BriefingMailDto

    var body: some View {
        if let thread = mail.threadId {
            NavigationLink {
                ThreadView(threadId: thread)
            } label: {
                MailRow(mail: mail)
            }
        } else {
            MailRow(mail: mail)
        }
    }
}

private struct MailRow: View {
    let mail: BriefingMailDto

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline) {
                Text(mail.from)
                    .font(.subheadline.weight(mail.unread ? .semibold : .regular))
                    .lineLimit(1)
                Spacer(minLength: 4)
                Text(Date.fromUnix(mail.receivedAt), format: .relative(presentation: .named))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Text(mail.subject)
                .font(.subheadline)
                .lineLimit(1)
            if !mail.reason.isEmpty {
                Text(mail.reason)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }
            if !mail.warnings.isEmpty {
                Text(mail.warnings.joined(separator: "; "))
                    .font(.footnote)
                    .foregroundStyle(.orange)
                    .lineLimit(3)
            }
        }
        .padding(.vertical, 2)
    }
}

extension MailCategory {
    var label: String {
        switch self {
        case .person: "From people"
        case .newsletter: "Newsletters"
        case .notification: "Notifications"
        case .transactional: "Receipts and bills"
        case .list: "Mailing lists"
        case .other: "Other"
        }
    }
}
