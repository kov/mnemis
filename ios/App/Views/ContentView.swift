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
        .sheet(item: $model.composing, onDismiss: { model.draftChanged() }) { draft in
            ComposeView(draft: draft)
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

/// What the iPad sidebar can select: a section, or a mail folder.
private enum SidebarItem: Hashable {
    case section(AppTab)
    case mailbox(MailScope)
}

/// iPad: sections | a stack where the list's detail slides in over it (with
/// back button and swipe), as in the Claude app.
private struct SplitLayout: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView(columnVisibility: $model.columnVisibility) {
            List(selection: Binding(
                get: { Optional(model.selectedTab == .mail ? SidebarItem.mailbox(model.mailScope) : .section(model.selectedTab)) },
                set: { item in
                    switch item {
                    case let .section(tab): model.selectedTab = tab
                    case let .mailbox(scope):
                        if model.selectedTab != .mail || model.mailScope != scope {
                            model.showMail(scope)
                        }
                    case nil: break
                    }
                }
            )) {
                Label("Briefing", systemImage: "sun.horizon")
                    .tag(SidebarItem.section(.briefing))
                Label("To-dos", systemImage: "checklist")
                    .badge(model.todos.count)
                    .tag(SidebarItem.section(.actions))
                Label("Chats", systemImage: "bubble.left.and.text.bubble.right")
                    .tag(SidebarItem.section(.chat))
                Section("Mail") {
                    Label("All Inboxes", systemImage: "tray.2")
                        .badge(model.inboxUnread)
                        .tag(SidebarItem.mailbox(.allInboxes))
                }
                ForEach(model.mailAccounts, id: \.account) { account in
                    Section(account.account) {
                        ForEach(account.folders, id: \.channelId) { box in
                            MailboxLabel(box: box).tag(SidebarItem.mailbox(box.scope))
                        }
                    }
                }
            }
            .navigationTitle("mnemis")
            .safeAreaInset(edge: .bottom) {
                VStack(alignment: .leading, spacing: 12) {
                    LearningNote()
                    // Floats at the sidebar's foot, as in the Claude app.
                    Button { model.selectedTab = .settings } label: { ProfileBadge() }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Settings")
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding()
            }
        } detail: {
            switch model.selectedTab {
            case .briefing: BriefingStack()
            case .actions: ActionsStack()
            case .chat: ChatsStack()
            case .settings: NavigationStack { SettingsView() }
            case .mail:
                // A new folder starts a new stack, rather than pushing.
                NavigationStack { ThreadListView(scope: model.mailScope) }
                    .id(model.mailScope)
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
            Tab("Briefing", systemImage: "sun.horizon", value: AppTab.briefing) {
                BriefingStack()
            }
            Tab("To-dos", systemImage: "checklist", value: AppTab.actions) {
                ActionsStack()
            }
            .badge(model.todos.count)
            Tab("Chats", systemImage: "bubble.left.and.text.bubble.right", value: AppTab.chat) {
                ChatsStack()
            }
            Tab("Mail", systemImage: "envelope", value: AppTab.mail) {
                NavigationStack(path: $model.mailPath) {
                    MailboxListView()
                        .navigationTitle("Mail")
                }
            }
            .badge(model.inboxUnread)
            Tab("Settings", systemImage: "gearshape", value: AppTab.settings) {
                NavigationStack { SettingsView() }
            }
        }
        .tabBarMinimizeBehavior(.onScrollDown)
    }
}

/// The action list; picking one pushes its detail.
private struct ActionsStack: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            ActionListView(selection: $model.actionSelection)
                .navigationTitle("To-dos")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .topBarTrailing) {
                        NavigationLink {
                            ResolvedActionsView()
                        } label: {
                            Label("Recently done", systemImage: "clock.arrow.circlepath")
                        }
                    }
                    SyncItem()
                }
                .navigationDestination(item: $model.actionSelection) { id in
                    ActionDetailView(actionId: id)
                }
        }
    }
}

/// The chat list; picking one pushes the conversation.
private struct ChatsStack: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            ChatListView(selection: $model.chatSelection)
                .navigationTitle("Chats")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
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

/// iPad: a floating glass circle with the user's initials, or a gear until
/// they give their name.
private struct ProfileBadge: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if model.initials.isEmpty {
                Image(systemName: "gearshape")
            } else {
                Text(model.initials)
                    .font(.headline)
            }
        }
        .frame(width: 48, height: 48)
        .contentShape(.circle)
        .glassEffect(.regular.interactive(), in: .circle)
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
            if !model.warnings.isEmpty {
                Section {
                    ForEach(model.warnings, id: \.id) { warning in
                        WarningRow(warning: warning)
                            .tag(warning.id)
                            .swipeActions(edge: .leading) {
                                Button("To-do", systemImage: "checklist") {
                                    Task { await model.accept(warning) }
                                }
                                .tint(.blue)
                            }
                            .swipeActions(edge: .trailing) {
                                Button("Dismiss", systemImage: "xmark") {
                                    Task { await model.discard(warning) }
                                }
                                .tint(.gray)
                            }
                    }
                } header: {
                    Text("Suspicious emails")
                }
            }
            if !model.overdue.isEmpty {
                Section {
                    ForEach(model.overdue, id: \.id) { action in
                        OverdueRow(action: action)
                            .tag(action.id)
                            .listRowBackground(Color.red.opacity(0.08))
                    }
                } header: {
                    Label("Overdue", systemImage: "exclamationmark.circle.fill")
                        .foregroundStyle(.red)
                }
            }
            Section {
                ForEach(model.current, id: \.id) { action in
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
            } header: {
                if (!model.warnings.isEmpty || !model.overdue.isEmpty) && !model.current.isEmpty {
                    Text("To-dos")
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

/// An overdue to-do, with Done and Postpone at hand.
struct OverdueRow: View {
    @Environment(AppModel.self) private var model
    let action: ActionDto

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ActionRow(action: action)
            HStack(spacing: 10) {
                Button("Done", systemImage: "checkmark") {
                    Task { await model.setStatus(action, to: .done) }
                }
                .tint(.green)
                Menu {
                    Button("Tomorrow") { Task { await model.postpone(action, days: 1) } }
                    Button("In 3 days") { Task { await model.postpone(action, days: 3) } }
                    Button("Next week") { Task { await model.postpone(action, days: 7) } }
                } label: {
                    Label("Postpone", systemImage: "calendar.badge.clock")
                }
                .tint(.orange)
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
        }
    }
}

struct WarningRow: View {
    let warning: ActionDto

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label {
                Text(warning.title)
                    .font(.body.weight(.medium))
                    .lineLimit(2)
            } icon: {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
            }
            HStack(spacing: 6) {
                Text("Looks like phishing")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer(minLength: 0)
                if let posted = warning.sourcePostedAt {
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
