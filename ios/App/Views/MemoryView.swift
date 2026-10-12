import SwiftUI

/// The rules mnemis has learned from your dismissals, editable. Your edit is
/// what the next merge builds on.
struct MemoryView: View {
    @Environment(AppModel.self) private var model
    @State private var rules = ""
    @State private var saved = ""
    @State private var loaded = false
    @State private var watches: [MailWatch] = []

    var body: some View {
        Form {
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
                    .disabled(rules == saved)
            }
        }
        .task { await load() }
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            rules = try await engine.memoryRules() ?? ""
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
        } catch {
            model.report(error)
        }
    }
}
