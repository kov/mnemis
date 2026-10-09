import SwiftUI

struct ChatsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        @Bindable var model = model
        let compact = sizeClass == .compact
        NavigationSplitView(columnVisibility: $model.columnVisibility) {
            List(selection: $model.chatSelection) {
                ForEach(model.chats) { chat in
                    ChatRow(chat: chat)
                        .tag(chat.id)
                        .swipeActions {
                            Button("Delete", systemImage: "trash", role: .destructive) {
                                Task { await model.deleteChat(chat) }
                            }
                        }
                }
            }
            .overlay {
                if model.chats.isEmpty {
                    ContentUnavailableView(
                        "No chats",
                        systemImage: "bubble.left.and.text.bubble.right",
                        description: Text("Ask about your mail and actions, or use “Talk about this” on an action.")
                    )
                }
            }
            .listColumnChrome(title: "Chat")
            .trailingControls(when: compact) { NewChatButton() }
            .refreshable { await model.refreshChats() }
        } detail: {
            Group {
                if let id = model.chatSelection {
                    ChatView(chatId: id)
                        .id(id)
                } else {
                    ContentUnavailableView("No chat selected", systemImage: "bubble.left.and.text.bubble.right")
                }
            }
            .trailingControls(when: !compact) { NewChatButton() }
        }
    }
}

private struct NewChatButton: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Button("New chat", systemImage: "square.and.pencil") {
            Task { await model.newChat() }
        }
        .disabled(model.engine == nil)
    }
}

private struct ChatRow: View {
    let chat: ChatDto

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: chat.seededFromKind == "action" ? "checklist" : "bubble.left.and.text.bubble.right")
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(Color.accentColor)
                .frame(width: 34, height: 34)
                .background(Color.accentColor.opacity(0.14), in: .circle)
            VStack(alignment: .leading, spacing: 2) {
                Text(chat.title ?? "New chat")
                    .font(.body.weight(.medium))
                    .lineLimit(2)
                Text(Date.fromUnix(chat.updatedAt), format: .relative(presentation: .named))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }
}
