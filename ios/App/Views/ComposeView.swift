import SwiftUI

/// Writing a mail: new, a reply or forward, or a draft the assistant wrote.
/// Nothing goes out until the user taps Send.
struct ComposeView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var draft: DraftDto
    @State private var showsCopies: Bool
    @State private var sending = false
    @State private var confirmingCancel = false
    @State private var failure: String?

    init(draft: DraftDto) {
        _draft = State(initialValue: draft)
        _showsCopies = State(initialValue: !draft.cc.isEmpty || !draft.bcc.isEmpty)
    }

    private var title: String {
        switch draft.kind {
        case .new: "New email"
        case .reply, .replyAll: "Reply"
        case .forward: "Forward"
        }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    if draft.fromChoices.count > 1 {
                        Picker("From", selection: $draft.fromAddress) {
                            ForEach(draft.fromChoices, id: \.self) { address in
                                Text(address).tag(address)
                            }
                        }
                    } else {
                        LabeledContent(
                            "From",
                            value: draft.fromAddress.isEmpty ? "No sender address" : draft.fromAddress
                        )
                    }
                    addressField("To", text: $draft.to)
                    if showsCopies {
                        addressField("Cc", text: $draft.cc)
                        addressField("Bcc", text: $draft.bcc)
                    } else {
                        Button("Add Cc or Bcc") { showsCopies = true }
                    }
                    LabeledContent("Subject") {
                        TextField("Subject", text: $draft.subject)
                    }
                }
                Section {
                    TextEditor(text: $draft.body)
                        .frame(minHeight: 300)
                } header: {
                    if draft.byAgent {
                        Label("Drafted by the assistant", systemImage: "sparkles")
                    }
                } footer: {
                    if draft.kind == .forward {
                        Text("The original email and its attachments go along when you send.")
                    }
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { confirmingCancel = true }
                        .disabled(sending)
                }
                ToolbarItem(placement: .confirmationAction) {
                    if sending {
                        ProgressView()
                    } else {
                        Button("Send", systemImage: "paperplane.fill") { Task { await send() } }
                            .disabled(draft.to.isEmpty && draft.cc.isEmpty && draft.bcc.isEmpty)
                    }
                }
            }
            .confirmationDialog("Close this email?", isPresented: $confirmingCancel) {
                if draft.chatId != nil {
                    Button("Keep the draft") { Task { await save(andClose: true) } }
                }
                Button("Discard", role: .destructive) { Task { await discard() } }
            } message: {
                if draft.chatId != nil {
                    Text("A kept draft stays in the conversation, ready to send later.")
                }
            }
            .alert(
                "Not sent",
                isPresented: Binding(get: { failure != nil }, set: { if !$0 { failure = nil } }),
                presenting: failure
            ) { _ in
                Button("OK", role: .cancel) {}
            } message: { message in
                Text(message)
            }
        }
        .interactiveDismissDisabled()
    }

    private func addressField(_ label: String, text: Binding<String>) -> some View {
        LabeledContent(label) {
            TextField(label, text: text)
                .keyboardType(.emailAddress)
                .textContentType(.emailAddress)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
        }
    }

    private func save(andClose: Bool) async {
        guard let engine = model.engine else { return }
        do {
            try await engine.saveDraft(draft: draft)
            if andClose { dismiss() }
        } catch {
            failure = message(error)
        }
    }

    private func send() async {
        guard let engine = model.engine else { return }
        sending = true
        defer { sending = false }
        do {
            try await engine.saveDraft(draft: draft)
            let sent = try await engine.sendDraft(draftId: draft.id)
            if let note = sent.error {
                // Sent, but filing the copy failed: worth knowing, not a failure.
                model.errorMessage = note
            }
            dismiss()
        } catch {
            failure = message(error)
        }
    }

    private func discard() async {
        guard let engine = model.engine else { return }
        do {
            try await engine.discardDraft(draftId: draft.id)
        } catch {
            model.report(error)
        }
        dismiss()
    }

    private func message(_ error: Error) -> String {
        if case let MnemisError.Failed(message) = error { message } else { error.localizedDescription }
    }
}
