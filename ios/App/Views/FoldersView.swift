import SwiftUI

/// Which of an account's folders mnemis reads. New folders start with only the
/// inbox and the sent folder subscribed; changes apply immediately.
struct FoldersView: View {
    @Environment(AppModel.self) private var model
    let source: SourceRowDto

    @State private var folders: [ChannelRowDto]?

    var body: some View {
        List {
            if let folders {
                Section {
                    ForEach(folders, id: \.id) { folder in
                        Toggle(isOn: subscribed(folder)) {
                            VStack(alignment: .leading, spacing: 2) {
                                Text(Self.displayName(folder.name))
                                if folder.messageCount > 0 {
                                    Text("\(folder.messageCount) messages read")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                        }
                    }
                } footer: {
                    Text("mnemis reads new mail in the folders that are on. Turning one on reads its 50 most recent messages on the next sync.")
                }
            }
        }
        .overlay {
            if folders == nil {
                ProgressView()
            } else if folders?.isEmpty == true {
                ContentUnavailableView(
                    "No folders",
                    systemImage: "folder",
                    description: Text("The server couldn't be reached. Check the account and try again.")
                )
            }
        }
        .navigationTitle(source.name)
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .refreshable { await load() }
    }

    private func subscribed(_ folder: ChannelRowDto) -> Binding<Bool> {
        Binding(
            get: { !folder.muted },
            set: { on in Task { await set(folder, subscribed: on) } }
        )
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            let all = try await engine.listFolders(sourceId: source.id)
            // The inbox first; the engine already sorts the rest by name.
            folders = all.filter { $0.name.uppercased() == "INBOX" }
                + all.filter { $0.name.uppercased() != "INBOX" }
        } catch {
            folders = []
            model.report(error)
        }
    }

    private func set(_ folder: ChannelRowDto, subscribed: Bool) async {
        guard let engine = model.engine else { return }
        do {
            try await engine.setFolderSubscribed(channelId: folder.id, subscribed: subscribed)
            if let i = folders?.firstIndex(where: { $0.id == folder.id }) {
                folders?[i].muted = !subscribed
            }
        } catch {
            model.report(error)
        }
    }

    static func displayName(_ name: String) -> String {
        name.uppercased() == "INBOX" ? "Inbox" : name
    }
}
