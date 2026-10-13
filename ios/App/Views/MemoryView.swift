import SwiftUI

/// What mnemis keeps about you, editable: your interests, which the briefing
/// judges mail by, and the rules learned from your dismissals (your edit is
/// what the next merge builds on).
struct MemoryView: View {
    @Environment(AppModel.self) private var model
    @State private var rules = ""
    @State private var saved = ""
    @State private var interests = ""
    @State private var savedInterests = ""
    @State private var loaded = false
    @State private var watches: [MailWatch] = []

    var body: some View {
        Form {
            Section {
                TextEditor(text: $interests)
                    .font(.callout)
                    .frame(minHeight: 120)
                    .overlay(alignment: .topLeading) {
                        if loaded && interests.isEmpty {
                            Text("Nothing yet. Write what you want to hear about, or tell the assistant in a chat.")
                                .font(.callout)
                                .foregroundStyle(.tertiary)
                                .padding(.top, 8)
                                .padding(.leading, 5)
                                .allowsHitTesting(false)
                        }
                    }
            } header: {
                Text("Interests")
            } footer: {
                Text("What you want to hear about in your mail, and what you don't. The briefing judges new mail by it.")
            }
            Section {
                TextEditor(text: $rules)
                    .font(.callout)
                    .frame(minHeight: 240)
                    .overlay(alignment: .topLeading) {
                        if loaded && rules.isEmpty {
                            Text("Nothing yet. When you dismiss a to-do and say why, mnemis writes a rule here.")
                                .font(.callout)
                                .foregroundStyle(.tertiary)
                                .padding(.top, 8)
                                .padding(.leading, 5)
                                .allowsHitTesting(false)
                        }
                    }
            } header: {
                Text("To-do rules")
            } footer: {
                Text("One rule per line. mnemis follows these when it reads your mail and in chat, and updates them each time you explain a dismissal.")
            }
            Section {
                if watches.isEmpty {
                    Text("Nothing. Ask in a chat, e.g. \u{201c}tell me when the confirmation arrives\u{201d}.")
                        .foregroundStyle(.secondary)
                }
                ForEach(watches, id: \.id) { watch in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(watch.lookingFor)
                        Text(watch.instruction)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        if let until = watch.expiresAt {
                            Text("Until \(Date.fromUnix(until).formatted(date: .abbreviated, time: .omitted))")
                                .font(.caption2)
                                .foregroundStyle(.tertiary)
                        }
                    }
                    .swipeActions {
                        Button("Stop", systemImage: "eye.slash", role: .destructive) {
                            Task { await cancel(watch) }
                        }
                    }
                }
            } header: {
                Text("Watching for")
            } footer: {
                Text("Mail you asked mnemis to look out for. When it arrives, mnemis does what you asked and says so in the chat.")
            }
        }
        .navigationTitle("Memory")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                Button("Save") { Task { await save() } }
                    .disabled(rules == saved && interests == savedInterests)
            }
        }
        .task { await load() }
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            rules = try await engine.memoryRules() ?? ""
            interests = try await engine.interests() ?? ""
            savedInterests = interests
            watches = try await engine.mailWatches()
            saved = rules
            loaded = true
        } catch {
            model.report(error)
        }
    }

    private func cancel(_ watch: MailWatch) async {
        guard let engine = model.engine else { return }
        do {
            try await engine.cancelMailWatch(id: watch.id)
            watches = try await engine.mailWatches()
        } catch {
            model.report(error)
        }
    }

    private func save() async {
        guard let engine = model.engine else { return }
        do {
            try await engine.saveMemoryRules(rules: rules)
            saved = rules
            try await engine.saveInterests(interests: interests)
            savedInterests = interests
        } catch {
            model.report(error)
        }
    }
}
