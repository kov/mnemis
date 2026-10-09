import SwiftUI

/// Asked right after a dismissal: why isn't this an action item? A reason is
/// merged into what mnemis has learned; Skip records nothing.
struct DismissReasonSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var close
    let action: ActionDto
    @State private var reason = ""
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text(action.title)
                        .foregroundStyle(.secondary)
                }
                Section {
                    TextField("For example: newsletters from this sender never need anything from me", text: $reason, axis: .vertical)
                        .lineLimit(3...8)
                        .focused($focused)
                } header: {
                    Text("Why isn't this an action item?")
                } footer: {
                    Text("Optional. mnemis turns your reason into a rule it follows from now on — see Settings → Memory.")
                }
            }
            .navigationTitle("Dismissed")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Skip") { close() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Teach") {
                        let text = reason
                        Task { await model.teach(action, reason: text) }
                        close()
                    }
                    .disabled(reason.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .onAppear { focused = true }
        }
        .presentationDetents([.medium, .large])
    }
}
