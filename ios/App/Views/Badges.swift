import SwiftUI

/// A small tinted capsule: an icon and a word.
struct Badge: View {
    let text: String
    let systemImage: String
    let tint: Color

    var body: some View {
        Label(text, systemImage: systemImage)
            .labelStyle(.titleAndIcon)
            .font(.caption.weight(.semibold))
            .lineLimit(1)
            .foregroundStyle(tint)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(tint.opacity(0.14), in: .capsule)
            .fixedSize()
    }
}

struct ConfidenceBadge: View {
    let confidence: Confidence

    var body: some View {
        switch confidence {
        case .high: Badge(text: "High", systemImage: "checkmark.seal.fill", tint: .green)
        case .medium: Badge(text: "Medium", systemImage: "circle.lefthalf.filled", tint: .orange)
        case .low: Badge(text: "Low", systemImage: "circle.dotted", tint: .secondary)
        }
    }
}

/// When it's due; red once past, orange within two days.
struct DueBadge: View {
    let due: Int64

    var body: some View {
        let date = Date.fromUnix(due)
        let remaining = date.timeIntervalSinceNow
        let tint: Color = remaining < 0 ? .red : remaining < 2 * 86_400 ? .orange : .blue
        Badge(
            text: date.formatted(.dateTime.day().month(.abbreviated)),
            systemImage: "calendar",
            tint: tint
        )
    }
}

/// A rounded panel on the grouped background.
struct Card<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
        .background(Color(.secondarySystemGroupedBackground), in: .rect(cornerRadius: 22))
    }
}
