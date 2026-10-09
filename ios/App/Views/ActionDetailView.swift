import SwiftUI

struct ActionDetailView: View {
    @Environment(AppModel.self) private var model
    let action: ActionDto
    @State private var message: MessageDetailDto?

    var body: some View {
        List {
            Section {
                Text(action.title)
                    .font(.title3.weight(.semibold))
                if let details = action.details, !details.isEmpty {
                    Text(details)
                }
                LabeledContent("Confidence") { ConfidenceBadge(confidence: action.confidence) }
                if let due = action.dueAt {
                    LabeledContent("Due", value: Date.fromUnix(due).formatted(date: .long, time: .omitted))
                }
            }

            Section {
                Button("Mark done", systemImage: "checkmark.circle") {
                    Task { await model.setStatus(action, to: .done) }
                }
                Button("Dismiss", systemImage: "xmark.circle", role: .destructive) {
                    Task { await model.setStatus(action, to: .dismissed) }
                }
            }

            if let message {
                Section("Original email") {
                    if let subject = message.subject {
                        Text(subject).font(.headline)
                    }
                    if let author = message.authorDisplay ?? message.authorAddr {
                        LabeledContent("From", value: author)
                    }
                    LabeledContent("Sent", value: Date.fromUnix(message.postedAt).formatted())
                    Text(message.body)
                        .font(.callout)
                        .textSelection(.enabled)
                }
            }
        }
        .navigationTitle("Action")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: action.id) { await loadMessage() }
    }

    private func loadMessage() async {
        message = nil
        guard let engine = model.engine, let id = action.sourceMessageId else { return }
        do {
            message = try await engine.getMessage(messageId: id)
        } catch {
            model.report(error)
        }
    }
}
