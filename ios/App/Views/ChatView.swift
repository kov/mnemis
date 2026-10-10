import SwiftUI

struct ChatView: View {
    @Environment(AppModel.self) private var model
    @State private var session: ChatSession
    @State private var draft = ""
    @FocusState private var composerFocused: Bool
    @State private var position = ScrollPosition(edge: .bottom)
    /// Keep the newest lines in view as the transcript grows, until the
    /// reader scrolls up.
    @State private var followsBottom = true

    init(chatId: Int64) {
        _session = State(initialValue: ChatSession(chatId: chatId))
    }

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                if let seed = session.seedLabel {
                    Label(seed, systemImage: "checklist")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.bottom, 4)
                }
                ForEach(TranscriptItem.build(session.turns)) { item in
                    TranscriptItemView(item: item)
                }
                if let pending = session.pendingUserText {
                    UserBubble(text: pending)
                }
                if !session.liveText.isEmpty {
                    MarkdownText(session.liveText)
                }
                if session.sending && session.liveText.isEmpty {
                    Label(
                        session.compacting ? "Condensing the conversation…" : "Thinking…",
                        systemImage: "ellipsis"
                    )
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .symbolEffect(.variableColor.iterative)
                }
            }
            .padding()
            .frame(maxWidth: 760)
            .frame(maxWidth: .infinity)
        }
        .scrollPosition($position)
        // Heights settle after the turns load (lazy rows, the push
        // animation), so a one-off scroll to the end stops short and leaves
        // the last lines under the composer. Re-pin whenever the content
        // grows instead.
        .onScrollGeometryChange(for: CGFloat.self) { $0.contentSize.height } action: { _, _ in
            if followsBottom { position.scrollTo(edge: .bottom) }
        }
        .onScrollPhaseChange { _, phase, context in
            guard phase == .idle else { return }
            let geometry = context.geometry
            let end = geometry.contentSize.height + geometry.contentInsets.bottom
                - geometry.containerSize.height
            followsBottom = geometry.contentOffset.y >= end - 40
        }
        .scrollDismissesKeyboard(.interactively)
        // A safe-area bar, unlike an inset, gets the scroll edge effect.
        .safeAreaBar(edge: .bottom) { composer }
        .navigationTitle(model.chats.first(where: { $0.id == session.chatId })?.title ?? "Chat")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            guard let engine = model.engine else { return }
            await session.load(engine, model: model)
        }
    }

    /// A floating glass field with its send/stop button, over the transcript.
    private var composer: some View {
        GlassEffectContainer(spacing: 10) {
            HStack(alignment: .bottom, spacing: 10) {
                TextField("Ask about your mail…", text: $draft, axis: .vertical)
                    .lineLimit(1...6)
                    .focused($composerFocused)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 11)
                    .glassEffect(.regular.interactive(), in: .rect(cornerRadius: 22))
                Group {
                    if session.sending {
                        Button {
                            if let engine = model.engine { session.stop(engine) }
                        } label: {
                            Image(systemName: "stop.fill")
                                .frame(width: 24, height: 24)
                        }
                        .accessibilityLabel("Stop")
                    } else {
                        Button { send() } label: {
                            Image(systemName: "arrow.up")
                                .font(.body.weight(.semibold))
                                .frame(width: 24, height: 24)
                        }
                        .accessibilityLabel("Send")
                        .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
                .buttonStyle(.glassProminent)
                .buttonBorderShape(.circle)
                .controlSize(.large)
            }
        }
        .padding(.horizontal)
        .padding(.bottom, 8)
        .frame(maxWidth: 760)
    }

    private func send() {
        guard let engine = model.engine else { return }
        let text = draft
        draft = ""
        followsBottom = true
        Task { await session.send(text, engine: engine, model: model) }
    }
}

/// What the transcript shows: turns, with each tool call paired with its result.
enum TranscriptItem: Identifiable {
    case user(id: Int64, text: String)
    case assistant(id: Int64, text: String, reasoning: String?)
    case reasoning(id: Int64, text: String)
    case tool(id: Int64, name: String, arguments: String, output: String?)

    var id: Int64 {
        switch self {
        case let .user(id, _), let .assistant(id, _, _), let .reasoning(id, _),
             let .tool(id, _, _, _):
            id
        }
    }

    static func build(_ turns: [ChatTurnDto]) -> [TranscriptItem] {
        var outputs: [String: String] = [:]
        for turn in turns where turn.role == "tool" {
            if let callId = turn.toolCallId { outputs[callId] = turn.content ?? "" }
        }
        var items: [TranscriptItem] = []
        for turn in turns {
            switch turn.role {
            case "user":
                items.append(.user(id: turn.id, text: turn.content ?? ""))
            case "assistant" where turn.toolName != nil:
                if let reasoning = turn.reasoning, !reasoning.isEmpty {
                    items.append(.reasoning(id: -turn.id, text: reasoning))
                }
                items.append(.tool(
                    id: turn.id,
                    name: turn.toolName ?? "",
                    arguments: turn.content ?? "",
                    output: turn.toolCallId.flatMap { outputs[$0] }
                ))
            case "assistant":
                items.append(.assistant(id: turn.id, text: turn.content ?? "", reasoning: turn.reasoning))
            default:
                break // tool results ride on their call
            }
        }
        return items
    }
}

private struct TranscriptItemView: View {
    let item: TranscriptItem

    var body: some View {
        switch item {
        case let .user(_, text):
            UserBubble(text: text)
        case let .assistant(_, text, reasoning):
            VStack(alignment: .leading, spacing: 6) {
                if let reasoning, !reasoning.isEmpty {
                    ReasoningView(text: reasoning)
                }
                if !text.isEmpty {
                    MarkdownText(text)
                }
            }
        case let .reasoning(_, text):
            ReasoningView(text: text)
        case let .tool(_, name, arguments, output):
            ToolCallView(name: name, arguments: arguments, output: output)
        }
    }
}

private struct UserBubble: View {
    let text: String

    var body: some View {
        Text(text)
            .textSelection(.enabled)
            .padding(.horizontal, 14)
            .padding(.vertical, 9)
            .foregroundStyle(.white)
            .background(Color.accentColor, in: .rect(cornerRadius: 18))
            .frame(maxWidth: .infinity, alignment: .trailing)
            .padding(.leading, 40)
    }
}

struct MarkdownText: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(Self.render(text))
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// Inline markdown (bold, italics, code, links) with line breaks kept;
    /// block syntax shows as written.
    static func render(_ text: String) -> AttributedString {
        let options = AttributedString.MarkdownParsingOptions(
            interpretedSyntax: .inlineOnlyPreservingWhitespace
        )
        return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(text)
    }
}

private struct ReasoningView: View {
    let text: String

    var body: some View {
        DisclosureGroup {
            Text(text)
                .font(.footnote)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        } label: {
            Label("Thinking", systemImage: "brain")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }
}

private struct ToolCallView: View {
    let name: String
    let arguments: String
    let output: String?

    var body: some View {
        DisclosureGroup {
            VStack(alignment: .leading, spacing: 8) {
                Text(arguments)
                    .font(.caption.monospaced())
                if let output {
                    Divider()
                    Text(output.count > 4000 ? String(output.prefix(4000)) + "…" : output)
                        .font(.caption.monospaced())
                }
            }
            .foregroundStyle(.secondary)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
        } label: {
            Label(Self.describe(name), systemImage: output == nil ? "hourglass" : "wrench.and.screwdriver")
                .font(.footnote)
                .foregroundStyle(.secondary)
        }
    }

    static func describe(_ tool: String) -> String {
        switch tool {
        case "search_messages": "Searched messages"
        case "fetch_messages": "Read messages"
        case "get_action": "Looked up a to-do"
        case "list_actions": "Listed to-dos"
        case "list_messages": "Listed messages"
        case "search_conversation": "Searched this conversation"
        case "recall_turns": "Recalled earlier messages"
        default: tool.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }
}
