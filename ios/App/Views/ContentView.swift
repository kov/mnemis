import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        // One window-wide header bar holds the app's controls; the sections
        // below drop their sidebar navigation bars.
        VStack(spacing: 0) {
            HeaderBar()
            Divider()
            Group {
                switch model.selectedTab {
                case .actions: ActionsView()
                case .chat: ChatsView()
                }
            }
            .frame(maxHeight: .infinity)
        }
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

/// The window-wide bar: sidebar toggle and Settings on the left, the section
/// switcher in the middle, the section's own actions on the right.
struct HeaderBar: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        HStack(spacing: 4) {
            HStack(spacing: 4) {
                if sizeClass == .regular {
                    HeaderButton("Sidebar", systemImage: "sidebar.leading") {
                        withAnimation {
                            model.columnVisibility =
                                model.columnVisibility == .detailOnly ? .all : .detailOnly
                        }
                    }
                }
                HeaderButton("Settings", systemImage: "gearshape") { model.showingSettings = true }
                if model.isLearning {
                    Label("Learning…", systemImage: "brain")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .symbolEffect(.pulse)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            SectionSwitcher()

            HStack(spacing: 4) {
                switch model.selectedTab {
                case .actions:
                    if model.isSyncing {
                        ProgressView().frame(width: 44, height: 44)
                    } else {
                        HeaderButton("Sync", systemImage: "arrow.triangle.2.circlepath") {
                            Task { await model.sync() }
                        }
                        .disabled(model.engine == nil)
                    }
                case .chat:
                    HeaderButton("New chat", systemImage: "square.and.pencil") {
                        Task { await model.newChat() }
                    }
                    .disabled(model.engine == nil)
                }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .padding(.horizontal, 8)
        .frame(height: 52)
        .background(.bar)
    }
}

private struct HeaderButton: View {
    let title: String
    let systemImage: String
    let action: () -> Void

    init(_ title: String, systemImage: String, action: @escaping () -> Void) {
        self.title = title
        self.systemImage = systemImage
        self.action = action
    }

    var body: some View {
        Button(title, systemImage: systemImage, action: action)
            .labelStyle(.iconOnly)
            .font(.title3)
            .frame(width: 44, height: 44)
            .contentShape(.rect)
    }
}

/// Actions | Chat.
struct SectionSwitcher: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Picker("Section", selection: $model.selectedTab) {
            Label("Actions", systemImage: "checklist")
                .tag(AppTab.actions)
            Label("Chat", systemImage: "bubble.left.and.text.bubble.right")
                .tag(AppTab.chat)
        }
        .pickerStyle(.segmented)
        .fixedSize()
    }
}

struct ActionsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView(columnVisibility: $model.columnVisibility) {
            ActionListView(selection: $model.actionSelection)
                .navigationTitle("Actions")
                .toolbar(.hidden, for: .navigationBar)
                // The header bar has the sidebar toggle.
                .toolbar(removing: .sidebarToggle)
        } detail: {
            if let id = model.actionSelection, let action = model.actions.first(where: { $0.id == id }) {
                ActionDetailView(action: action)
            } else {
                ContentUnavailableView("No action selected", systemImage: "checklist")
            }
        }
    }
}

struct ActionListView: View {
    @Environment(AppModel.self) private var model
    @Binding var selection: ActionDto.ID?

    var body: some View {
        List(selection: $selection) {
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
        VStack(alignment: .leading, spacing: 4) {
            Text(action.title)
                .font(.body)
                .lineLimit(2)
            HStack(spacing: 8) {
                ConfidenceBadge(confidence: action.confidence)
                if let due = action.dueAt {
                    Label(Date.fromUnix(due).formatted(date: .abbreviated, time: .omitted), systemImage: "calendar")
                }
                if let posted = action.sourcePostedAt {
                    Text(Date.fromUnix(posted), format: .relative(presentation: .named))
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

struct ConfidenceBadge: View {
    let confidence: Confidence

    var body: some View {
        switch confidence {
        case .high: Text("High").foregroundStyle(.green)
        case .medium: Text("Medium").foregroundStyle(.orange)
        case .low: Text("Low").foregroundStyle(.secondary)
        }
    }
}

extension Date {
    static func fromUnix(_ seconds: Int64) -> Date {
        Date(timeIntervalSince1970: TimeInterval(seconds))
    }
}
