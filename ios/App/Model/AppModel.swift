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
    /// The action just dismissed, while the "why?" sheet is up.
    var pendingDismissal: ActionDto?
    /// A dismissal reason is being merged into memory.
    private(set) var isLearning = false

    /// True until an LLM and at least one account are configured — the
    /// actions list shows a setup prompt instead of an empty list.
    private(set) var needsSetup = false

    func start() async {
        guard engine == nil else { return }
        do {
            let engine = try await Engine.open(
                dataDir: Self.dataDirectory.path,
                apple: AppleIntelligence()
            )
            self.engine = engine
            try await defaultToAppleIntelligence(engine)
            await refresh()
            #if DEBUG
            // `-MnemisOpenURL mnemis://…` at launch opens that screen, so the
            // simulator can be driven without taps (and without the
            // confirmation `simctl openurl` raises).
            if let link = UserDefaults.standard.string(forKey: "MnemisOpenURL"),
               let url = URL(string: link) {
                open(url)
            }
            // `-MnemisLLM provider:model` switches the LLM (e.g.
            // `apple-intelligence:on-device`), logging the one it replaces.
            if let choice = UserDefaults.standard.string(forKey: "MnemisLLM") {
                await switchLLM(choice, engine: engine)
            }
            // `-MnemisAsk "…"` starts a chat with that question, logging the
            // engine's events, so the model can be exercised without taps.
            // `-MnemisDeleteChat <id>` deletes a chat left over from a run.
            if let doomed = Int64(UserDefaults.standard.string(forKey: "MnemisDeleteChat") ?? "") {
                try? await engine.deleteChat(chatId: doomed)
                await refreshChats()
                print("[MnemisAsk] deleted chat \(doomed)")
            }
            // `-MnemisAskStop <s>` presses Stop that many seconds in, then
            // `-MnemisAskNext "…"` follows up; `-MnemisAskDelete YES`
            // deletes the chat afterwards.
            if let question = UserDefaults.standard.string(forKey: "MnemisAsk") {
                await ask(question, engine: engine)
            }
            #endif
        } catch {
            report(error)
        }
    }

    #if DEBUG
    private func switchLLM(_ choice: String, engine: Engine) async {
        let parts = choice.split(separator: ":", maxSplits: 1).map(String.init)
        guard parts.count == 2 else { return }
        do {
            let llm = try await engine.llmConfig()
            print("[MnemisLLM] was \(llm.provider):\(llm.chatModel)")
            let provider: LlmProvider = switch parts[0] {
            case "apple-intelligence": .appleIntelligence
            case "anthropic": .anthropic
            default: .openaiResponses
            }
            try await engine.saveLlmConfig(cfg: LlmConfigDto(
                provider: provider,
                baseUrl: provider == .openaiResponses ? llm.baseUrl : "",
                chatModel: parts[1],
                bearerToken: llm.bearerToken,
                thinkingLevel: llm.thinkingLevel,
                anthropicApiKey: nil,
                anthropicApiKeySet: llm.anthropicApiKeySet,
                configPath: ""
            ))
            print("[MnemisLLM] now \(choice)")
        } catch {
            print("[MnemisLLM] failed: \(error)")
        }
    }

    private func ask(_ question: String, engine: Engine) async {
        let defaults = UserDefaults.standard
        do {
            let id = try await engine.createChat(seededFromKind: nil, seededFromId: nil)
            await refreshChats()
            if let stopAfter = defaults.object(forKey: "MnemisAskStop") as? Double ?? Double(defaults.string(forKey: "MnemisAskStop") ?? "") {
                Task {
                    try? await Task.sleep(for: .seconds(stopAfter))
                    print("[MnemisAsk] pressing Stop")
                    engine.cancelChatMessage(chatId: id)
                }
            }
            do {
                try await send(question, chat: id, engine: engine)
                if let next = defaults.string(forKey: "MnemisAskNext") {
                    try await send(next, chat: id, engine: engine)
                }
            } catch {
                print("[MnemisAsk] failed: \(error)")
            }
            if defaults.bool(forKey: "MnemisAskDelete") {
                try await engine.deleteChat(chatId: id)
                print("[MnemisAsk] deleted the chat")
            } else {
                open(URL(string: "mnemis://chat/\(id)")!)
            }
            await refreshChats()
        } catch {
            print("[MnemisAsk] failed: \(error)")
            report(error)
        }
    }

    private func send(_ text: String, chat id: Int64, engine: Engine) async throws {
        let (events, continuation) = AsyncStream.makeStream(of: ChatEvent.self)
        let logger = Task {
            for await event in events { print("[MnemisAsk] \(event)") }
        }
        let started = Date()
        defer {
            continuation.finish()
            print("[MnemisAsk] send finished after \(Int(Date().timeIntervalSince(started)))s")
        }
        try await engine.sendChatMessage(chatId: id, text: text, listener: ChatEventRelay(continuation))
        continuation.finish()
        await logger.value
    }
    #endif

    /// A fresh install uses the on-device model, when the device has one.
    private func defaultToAppleIntelligence(_ engine: Engine) async throws {
        let llm = try await engine.llmConfig()
        guard llm.chatModel.isEmpty, AppleIntelligence.onDeviceAvailable else { return }
        try await engine.saveLlmConfig(cfg: LlmConfigDto(
            provider: .appleIntelligence,
            baseUrl: "",
            chatModel: AppleIntelligence.onDevice,
            bearerToken: nil,
            thinkingLevel: llm.thinkingLevel,
            anthropicApiKey: nil,
            anthropicApiKeySet: llm.anthropicApiKeySet,
            configPath: ""
        ))
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

    /// Dismiss at once, like the desktop, then ask why; the answer is optional.
    func dismiss(_ action: ActionDto) async {
        await setStatus(action, to: .dismissed)
        if actionSelection == action.id { actionSelection = nil }
        pendingDismissal = action
    }

    /// Record the reason and merge it into memory in the background.
    func teach(_ action: ActionDto, reason: String) async {
        let reason = reason.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let engine, !reason.isEmpty else { return }
        do {
            try await engine.recordDismissalReason(actionId: action.id, reason: reason)
        } catch {
            report(error)
            return
        }
        isLearning = true
        defer { isLearning = false }
        do {
            _ = try await engine.learnFromDismissal(actionId: action.id, reason: reason)
        } catch {
            report(error)
        }
    }

    /// Deep links: `mnemis://action/<id>` and `mnemis://chat/<id>` open that
    /// action or chat (for notifications, and for driving the app in tests).
    func open(_ url: URL) {
        guard url.scheme == "mnemis", let id = Int64(url.lastPathComponent) else { return }
        switch url.host() {
        case "action":
            selectedTab = .actions
            actionSelection = id
        case "chat":
            selectedTab = .chat
            chatSelection = id
        default:
            break
        }
    }

    func action(_ id: ActionDto.ID?) -> ActionDto? {
        guard let id else { return nil }
        return actions.first { $0.id == id }
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
