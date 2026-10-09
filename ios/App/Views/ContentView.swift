import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        TabView(selection: $model.selectedTab) {
            Tab("Actions", systemImage: "checklist", value: AppTab.actions) {
                ActionsView()
            }
            Tab("Chat", systemImage: "bubble.left.and.text.bubble.right", value: AppTab.chat) {
                ChatsView()
            }
        }
        .tabViewStyle(.sidebarAdaptable)
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

struct SettingsButton: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Button("Settings", systemImage: "gearshape") { model.showingSettings = true }
    }
}

struct ActionsView: View {
    @Environment(AppModel.self) private var model
    @State private var selection: ActionDto.ID?

    var body: some View {
        NavigationSplitView {
            ActionListView(selection: $selection)
                .navigationTitle("Actions")
                .toolbar {
                    ToolbarItem(placement: .topBarLeading) { SettingsButton() }
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
        } detail: {
            if let id = selection, let action = model.actions.first(where: { $0.id == id }) {
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
                            Task { await model.setStatus(action, to: .dismissed) }
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
