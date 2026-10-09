import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        // One window-wide header holds the app's controls, as glass over the
        // window's background; the sections below drop their sidebar bars.
        VStack(spacing: 0) {
            HeaderBar()
            Group {
                switch model.selectedTab {
                case .actions: ActionsView()
                case .chat: ChatsView()
                }
            }
            .frame(maxHeight: .infinity)
        }
        .background(Color(.systemGroupedBackground))
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

/// The window-wide header: sidebar toggle and Settings on the left, the
/// section switcher in the middle, the section's own actions on the right.
struct HeaderBar: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        HStack(spacing: 12) {
            GlassEffectContainer(spacing: 8) {
                HStack(spacing: 8) {
                    if sizeClass == .regular {
                        HeaderButton("Sidebar", systemImage: "sidebar.leading") {
                            withAnimation(.smooth) {
                                model.columnVisibility =
                                    model.columnVisibility == .detailOnly ? .all : .detailOnly
                            }
                        }
                    }
                    HeaderButton("Settings", systemImage: "gearshape") { model.showingSettings = true }
                    if model.isLearning {
                        Label("Learning…", systemImage: "brain")
                            .font(.footnote.weight(.medium))
                            .foregroundStyle(.secondary)
                            .symbolEffect(.pulse)
                            .padding(.horizontal, 12)
                            .frame(height: 40)
                            .glassEffect()
                            .transition(.opacity.combined(with: .scale))
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            SectionSwitcher()

            GlassEffectContainer(spacing: 8) {
                HStack(spacing: 8) {
                    switch model.selectedTab {
                    case .actions:
                        if model.isSyncing {
                            ProgressView()
                                .frame(width: 40, height: 40)
                                .glassEffect(in: .circle)
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
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .animation(.smooth, value: model.isLearning)
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
        Button(action: action) {
            Image(systemName: systemImage)
                .font(.body.weight(.medium))
                .frame(width: 24, height: 24)
        }
        .buttonStyle(.glass)
        .buttonBorderShape(.circle)
        .accessibilityLabel(title)
    }
}

/// Actions | Chat: a glass capsule whose selection slides between segments.
struct SectionSwitcher: View {
    @Environment(AppModel.self) private var model
    @Namespace private var selection

    var body: some View {
        HStack(spacing: 2) {
            segment(.actions, title: "Actions", systemImage: "checklist")
            segment(.chat, title: "Chat", systemImage: "bubble.left.and.text.bubble.right")
        }
        .padding(4)
        .glassEffect(.regular.interactive(), in: .capsule)
    }

    private func segment(_ tab: AppTab, title: String, systemImage: String) -> some View {
        let selected = model.selectedTab == tab
        return Button {
            withAnimation(.smooth(duration: 0.3)) { model.selectedTab = tab }
        } label: {
            Label(title, systemImage: systemImage)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(selected ? Color.accentColor : .primary)
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
                .background {
                    if selected {
                        Capsule()
                            .fill(Color.accentColor.opacity(0.16))
                            .matchedGeometryEffect(id: "selection", in: selection)
                    }
                }
                .contentShape(.capsule)
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
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
