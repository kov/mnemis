import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        // The system tab bar: a floating glass bar at the top on iPad (with
        // the draggable selection), at the bottom on iPhone. Each section's
        // controls sit in its own navigation bars, beside it.
        TabView(selection: $model.selectedTab) {
            Tab("Actions", systemImage: "checklist", value: AppTab.actions) {
                ActionsView()
            }
            Tab("Chat", systemImage: "bubble.left.and.text.bubble.right", value: AppTab.chat) {
                ChatsView()
            }
        }
        .tabViewStyle(.tabBarOnly)
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

/// Settings (and the "Learning…" note) at the window's leading edge, on the
/// list column.
struct LeadingControls: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        ToolbarItem(placement: .topBarLeading) {
            Button("Settings", systemImage: "gearshape") { model.showingSettings = true }
        }
        if model.isLearning {
            ToolbarItem(placement: .topBarLeading) {
                Label("Learning…", systemImage: "brain")
                    .labelStyle(.titleAndIcon)
                    .font(.footnote.weight(.medium))
                    .foregroundStyle(.secondary)
                    .symbolEffect(.pulse)
            }
        }
    }
}

/// A section's own actions sit at the window's trailing edge: on the detail
/// column side by side, on the list when the columns collapse (iPhone). The
/// size class is read outside the split view, since its list column always
/// reports compact.
extension View {
    func trailingControls(
        when shown: Bool,
        @ViewBuilder _ items: @escaping () -> some View
    ) -> some View {
        toolbar {
            if shown {
                ToolbarItem(placement: .topBarTrailing) { items() }
            }
        }
    }

    /// The list column: no title (the tab bar names the section) and the
    /// app-wide controls.
    func listColumnChrome(title: String) -> some View {
        navigationTitle(title)
            .toolbar(removing: .title)
            .toolbar { LeadingControls() }
    }
}

struct ActionsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        @Bindable var model = model
        let compact = sizeClass == .compact
        NavigationSplitView(columnVisibility: $model.columnVisibility) {
            ActionListView(selection: $model.actionSelection)
                .listColumnChrome(title: "Actions")
                .trailingControls(when: compact) { SyncButton() }
        } detail: {
            Group {
                if let id = model.actionSelection, let action = model.actions.first(where: { $0.id == id }) {
                    ActionDetailView(action: action)
                } else {
                    ContentUnavailableView("No action selected", systemImage: "checklist")
                }
            }
            .trailingControls(when: !compact) { SyncButton() }
        }
    }
}

private struct SyncButton: View {
    @Environment(AppModel.self) private var model

    var body: some View {
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
