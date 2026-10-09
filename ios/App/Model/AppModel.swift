import Foundation
import Observation
import SwiftUI

enum AppTab: Hashable {
    case actions
    case chat
}

/// The app's view of the engine: what the screens show, and the calls that
/// change it. Everything correct-or-not lives in the Rust engine; this only
/// keeps the latest answers around for SwiftUI.
@MainActor
@Observable
final class AppModel {
    private(set) var engine: Engine?
    private(set) var actions: [ActionDto] = []
    private(set) var sources: [SourceRowDto] = []
    private(set) var isSyncing = false
    private(set) var lastSync: SyncOutcome?
    private(set) var chats: [ChatDto] = []
    var errorMessage: String?

    var selectedTab: AppTab = .actions
    /// Kept here rather than in the views so switching sections keeps them.
    var actionSelection: ActionDto.ID?
    /// Shared by both sections' split views, driven by the header's toggle.
    var columnVisibility: NavigationSplitViewVisibility = .all
    var chatSelection: ChatDto.ID?
    var showingSettings = false

    /// True until an LLM and at least one account are configured — the
    /// actions list shows a setup prompt instead of an empty list.
    private(set) var needsSetup = false

    func start() async {
        guard engine == nil else { return }
        do {
            engine = try await Engine.open(dataDir: Self.dataDirectory.path)
            await refresh()
        } catch {
            report(error)
        }
    }

    func refresh() async {
        guard let engine else { return }
        do {
            actions = try await engine.listActions(includeResolved: false)
            sources = try await engine.listSources()
            chats = try await engine.listChats(includeArchived: false)
            let llm = try await engine.llmConfig()
            needsSetup = sources.isEmpty || llm.chatModel.isEmpty
        } catch {
            report(error)
        }
    }

    func sync() async {
        guard let engine, !isSyncing else { return }
        isSyncing = true
        defer { isSyncing = false }
        do {
            let outcome = try await engine.syncNow()
            lastSync = outcome
            if !outcome.errors.isEmpty {
                errorMessage = outcome.errors.joined(separator: "\n")
            }
        } catch {
            report(error)
        }
        await refresh()
    }

    func setStatus(_ action: ActionDto, to status: ActionStatus) async {
        guard let engine else { return }
        do {
            try await engine.setActionStatus(actionId: action.id, status: status, dismissedReason: nil)
        } catch {
            report(error)
        }
        await refresh()
    }

    func refreshChats() async {
        guard let engine else { return }
        do {
            chats = try await engine.listChats(includeArchived: false)
        } catch {
            report(error)
        }
    }

    /// Open the chat about `action` — the existing one if there is one — on
    /// the Chat tab.
    func talkAbout(_ action: ActionDto) async {
        guard let engine else { return }
        do {
            let id: Int64
            if let existing = try await engine.findSeededChat(seededFromKind: "action", seededFromId: action.id) {
                id = existing
            } else {
                id = try await engine.createChat(seededFromKind: "action", seededFromId: action.id)
            }
            await refreshChats()
            chatSelection = id
            selectedTab = .chat
        } catch {
            report(error)
        }
    }

    func newChat() async {
        guard let engine else { return }
        do {
            let id = try await engine.createChat(seededFromKind: nil, seededFromId: nil)
            await refreshChats()
            chatSelection = id
        } catch {
            report(error)
        }
    }

    func deleteChat(_ chat: ChatDto) async {
        guard let engine else { return }
        do {
            try await engine.deleteChat(chatId: chat.id)
            if chatSelection == chat.id { chatSelection = nil }
        } catch {
            report(error)
        }
        await refreshChats()
    }

    func report(_ error: Error) {
        if case let MnemisError.Failed(message) = error {
            errorMessage = message
        } else {
            errorMessage = error.localizedDescription
        }
    }

    /// `Application Support/mnemis`: the database, config.toml and traces.
    /// Not visible in Files, and backed up with the device.
    static var dataDirectory: URL {
        URL.applicationSupportDirectory.appending(path: "mnemis", directoryHint: .isDirectory)
    }
}
