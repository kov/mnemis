import SwiftUI

struct ChatsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
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
            .navigationTitle("Chat")
            .toolbar(.hidden, for: .navigationBar)
            .toolbar(removing: .sidebarToggle)
            .refreshable { await model.refreshChats() }
        } detail: {
            if let id = model.chatSelection {
                ChatView(chatId: id)
                    .id(id)
            } else {
                ContentUnavailableView("No chat selected", systemImage: "bubble.left.and.text.bubble.right")
            }
        }
    }
}

private struct ChatRow: View {
    let chat: ChatDto

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                if chat.seededFromKind == "action" {
                    Image(systemName: "checklist").foregroundStyle(.secondary)
                }
                Text(chat.title ?? "New chat")
                    .lineLimit(2)
            }
            Text(Date.fromUnix(chat.updatedAt), format: .relative(presentation: .named))
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}
