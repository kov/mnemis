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
            Tab(value: AppTab.settings) {
                NavigationStack { SettingsView() }
            } label: {
                Label {
                    Text("Settings")
                } icon: {
                    ProfileIcon()
                }
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

/// The tab bar's profile icon: the user's initials in a ring, or a gear
/// until they give their name. Drawn as an image, as the tab bar shows only
/// images there.
private struct ProfileIcon: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if model.initials.isEmpty {
            Image(systemName: "gearshape")
        } else {
            Image(uiImage: Self.ring(model.initials))
        }
    }

    private static func ring(_ initials: String) -> UIImage {
        let size = CGSize(width: 28, height: 28)
        let image = UIGraphicsImageRenderer(size: size).image { _ in
            let bounds = CGRect(origin: .zero, size: size).insetBy(dx: 1, dy: 1)
            let ring = UIBezierPath(ovalIn: bounds)
            ring.lineWidth = 1.5
            UIColor.black.setStroke()
            ring.stroke()
            let font = UIFont.systemFont(ofSize: initials.count > 1 ? 11 : 13, weight: .bold)
            let text = NSAttributedString(string: initials, attributes: [.font: font, .foregroundColor: UIColor.black])
            let textSize = text.size()
            text.draw(at: CGPoint(x: (size.width - textSize.width) / 2, y: (size.height - textSize.height) / 2))
        }
        // Tinted like the other tabs' icons, selected or not.
        return image.withRenderingMode(.alwaysTemplate)
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
            Section {
                ForEach(model.todos, id: \.id) { action in
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
                if !model.warnings.isEmpty && !model.todos.isEmpty {
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
