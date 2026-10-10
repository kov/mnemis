import SwiftUI

/// The rules mnemis has learned from your dismissals, editable. Your edit is
/// what the next merge builds on.
struct MemoryView: View {
    @Environment(AppModel.self) private var model
    @State private var rules = ""
    @State private var saved = ""
    @State private var loaded = false

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
            saved = rules
            loaded = true
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
