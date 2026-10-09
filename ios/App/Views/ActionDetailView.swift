import SwiftUI

struct ActionDetailView: View {
    @Environment(AppModel.self) private var model
    let action: ActionDto
    @State private var message: MessageDetailDto?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Card {
                    HStack(spacing: 6) {
                        ConfidenceBadge(confidence: action.confidence)
                        if let due = action.dueAt {
                            DueBadge(due: due)
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

                GlassEffectContainer(spacing: 10) {
                    // One row when it fits; otherwise Done and Dismiss share a
                    // row and "Talk about this" takes the next.
                    ViewThatFits(in: .horizontal) {
                        HStack(spacing: 10) {
                            doneButton
                            talkButton
                            dismissButton
                        }
                        VStack(spacing: 10) {
                            HStack(spacing: 10) {
                                doneButton
                                dismissButton
                            }
                            talkButton
                        }
                    }
                    .controlSize(.large)
                    .labelStyle(.titleAndIcon)
                    .font(.subheadline.weight(.semibold))
                }

                if let message {
                    Card {
                        Label("Original email", systemImage: "envelope")
                            .font(.footnote.weight(.semibold))
                            .foregroundStyle(.secondary)
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
        .navigationTitle("Action")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: action.id) { await loadMessage() }
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
