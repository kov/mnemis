import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        @Bindable var model = model
        // The iOS 26 shapes: on iPad a split with the sections in the
        // floating sidebar; on iPhone a tab bar. Either way each section is a
        // navigation stack, list then detail. A top tab bar over a split
        // view is what Apple advises against: the two fight over the top edge.
        Group {
            if sizeClass == .compact {
                CompactLayout()
            } else {
                SplitLayout()
            }
        }
        .onOpenURL { model.open($0) }
        .sheet(item: $model.pendingDismissal) { action in
            DismissReasonSheet(action: action)
        }
        .sheet(isPresented: $model.showingSettings, onDismiss: { Task { await model.refresh() } }) {
            SettingsView()
        }
        .alert(
            "Something went wrong",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } }
            ),
            presenting: model.errorMessage
        ) { _ in
            Button("OK", role: .cancel) {}
        } message: { message in
            Text(message)
        }
    }
}

/// iPad: sections | a stack where the list's detail slides in over it (with
/// back button and swipe), as in the Claude app.
private struct SplitLayout: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView(columnVisibility: $model.columnVisibility) {
            List(selection: Binding(
                get: { Optional(model.selectedTab) },
                set: { if let tab = $0 { model.selectedTab = tab } }
            )) {
                Label("To-dos", systemImage: "checklist")
                    .badge(model.actions.count)
                    .tag(AppTab.actions)
                Label("Chats", systemImage: "bubble.left.and.text.bubble.right")
                    .tag(AppTab.chat)
            }
            .navigationTitle("mnemis")
            .toolbar { SettingsItem() }
            .safeAreaInset(edge: .bottom) { LearningNote().padding() }
        } detail: {
            switch model.selectedTab {
            case .actions: ActionsStack(showsSettings: false)
            case .chat: ChatsStack(showsSettings: false)
            }
        }
    }
}

/// iPhone: a tab per section, each the same stack as on iPad.
private struct CompactLayout: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        TabView(selection: $model.selectedTab) {
            Tab("To-dos", systemImage: "checklist", value: AppTab.actions) {
                ActionsStack(showsSettings: true)
            }
            .badge(model.actions.count)
            Tab("Chats", systemImage: "bubble.left.and.text.bubble.right", value: AppTab.chat) {
                ChatsStack(showsSettings: true)
            }
        }
        .tabBarMinimizeBehavior(.onScrollDown)
    }
}

/// The action list; picking one pushes its detail.
private struct ActionsStack: View {
    @Environment(AppModel.self) private var model
    /// On iPad Settings lives on the sidebar instead.
    let showsSettings: Bool

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            ActionListView(selection: $model.actionSelection)
                .navigationTitle("To-dos")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    if showsSettings { SettingsItem() }
                    SyncItem()
                }
                .navigationDestination(item: $model.actionSelection) { id in
                    if let action = model.action(id) {
                        ActionDetailView(action: action)
                    }
                }
        }
    }
}

/// The chat list; picking one pushes the conversation.
private struct ChatsStack: View {
    @Environment(AppModel.self) private var model
    let showsSettings: Bool

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            ChatListView(selection: $model.chatSelection)
                .navigationTitle("Chats")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    if showsSettings { SettingsItem() }
                    NewChatItem()
                }
                .navigationDestination(item: $model.chatSelection) { id in
                    // The composer takes the bottom edge, as in Messages.
                    ChatView(chatId: id).id(id)
                        .toolbar(.hidden, for: .tabBar)
                }
        }
    }
}

private struct SettingsItem: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Button("Settings", systemImage: "gearshape") { model.showingSettings = true }
        }
    }
}

private struct SyncItem: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            if model.isSyncing {
                ProgressView()
            } else {
                Button("Sync", systemImage: "arrow.triangle.2.circlepath") {
                    Task { await model.sync() }
                }
                .disabled(model.engine == nil)
            }
        }
    }
}

private struct NewChatItem: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            Button("New chat", systemImage: "square.and.pencil") {
                Task { await model.newChat() }
            }
            .disabled(model.engine == nil)
        }
    }
}

/// Shown while a dismissal reason is being merged into memory.
struct LearningNote: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.isLearning {
            Label("Learning from your feedback…", systemImage: "brain")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .symbolEffect(.pulse)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

struct ActionListView: View {
    @Environment(AppModel.self) private var model
    @Binding var selection: ActionDto.ID?

    var body: some View {
        List(selection: $selection) {
            if model.isLearning {
                LearningNote()
            }
            if model.needsSetup {
                Section {
                    Label(
                        "Add a mail account and a language model in Settings, then sync.",
                        systemImage: "sparkles"
                    )
                    .foregroundStyle(.secondary)
                }
            }
            ForEach(model.actions, id: \.id) { action in
                ActionRow(action: action)
                    .tag(action.id)
                    .swipeActions(edge: .leading) {
                        Button("Done", systemImage: "checkmark") {
                            Task { await model.setStatus(action, to: .done) }
                        }
                        .tint(.green)
                    }
                    .swipeActions(edge: .trailing) {
                        Button("Dismiss", systemImage: "xmark") {
                            Task { await model.dismiss(action) }
                        }
                        .tint(.gray)
                    }
            }
        }
        .overlay {
            if model.actions.isEmpty && !model.needsSetup {
                ContentUnavailableView(
                    "Nothing to do",
                    systemImage: "checkmark.circle",
                    description: Text("Pull down to check your mail.")
                )
            }
        }
        .refreshable { await model.sync() }
    }
}

struct ActionRow: View {
    let action: ActionDto

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(action.title)
                .font(.body.weight(.medium))
                .lineLimit(2)
            HStack(spacing: 6) {
                ConfidenceBadge(confidence: action.confidence)
                if let due = action.dueAt {
                    DueBadge(due: due)
                }
                Spacer(minLength: 0)
                if let posted = action.sourcePostedAt {
                    Text(Date.fromUnix(posted), format: .relative(presentation: .named))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.8)
                }
            }
        }
        .padding(.vertical, 4)
    }
}

extension Date {
    static func fromUnix(_ seconds: Int64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(seconds))
    }
}
