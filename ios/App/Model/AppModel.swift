import BackgroundTasks
import EventKit
import Foundation
import Network
import Observation
import SwiftUI

/// Asks iOS for time to finish work if the user leaves the app midway (it
/// grants about 30 seconds); without it the work freezes until they come
/// back, and its server connections die meanwhile.
@MainActor
final class FinishInBackground {
    private var id = UIBackgroundTaskIdentifier.invalid

    init(_ name: String) {
        id = UIApplication.shared.beginBackgroundTask(withName: name) { [weak self] in
            // Out of time: the work freezes where it is.
            MainActor.assumeIsolated { self?.end() }
        }
    }

    func end() {
        guard id != .invalid else { return }
        UIApplication.shared.endBackgroundTask(id)
        id = .invalid
    }
}

enum AppTab: Hashable {
    case actions
    case chat
    case mail
    /// iPad only; on iPhone Settings is pushed onto the current tab.
    case settings
}

/// The app's view of the engine: what the screens show, and the calls that
/// change it. Everything correct-or-not lives in the Rust engine; this only
/// keeps the latest answers around for SwiftUI.
@MainActor
@Observable
final class AppModel {
    private(set) var engine: Engine?
    private(set) var actions: [ActionDto] = []
    /// To-dos done, cancelled or dismissed lately, to bring one back.
    private(set) var resolved: [ActionDto] = []
    private(set) var sources: [SourceRowDto] = []
    private(set) var isSyncing = false
    private(set) var lastSync: SyncOutcome?
    private(set) var chats: [ChatDto] = []
    /// The user's initials (from their name in Settings), for the profile
    /// button; empty until they give a name.
    private(set) var initials = ""
    var errorMessage: String?

    var selectedTab: AppTab = .actions
    /// Kept here rather than in the views so switching sections keeps them.
    var actionSelection: ActionDto.ID?
    /// Shared by both sections' split views, driven by the header's toggle.
    var columnVisibility: NavigationSplitViewVisibility = .all
    var chatSelection: ChatDto.ID?
    /// The action just dismissed, while the "why?" sheet is up.
    var pendingDismissal: ActionDto?
    /// A dismissal reason is being merged into memory.
    private(set) var isLearning = false
    /// The mail being written, while the composer is up.
    var composing: DraftDto?
    /// Bumped whenever a draft changes, so views showing one reload it.
    private(set) var draftsVersion = 0

    /// True until an LLM and at least one account are configured — the
    /// actions list shows a setup prompt instead of an empty list.
    private(set) var needsSetup = false

    /// Every account's folders, for the mail sidebar.
    private(set) var mailboxes: [MailboxDto] = []
    /// The folder open in Mail on iPad.
    var mailScope: MailScope = .allInboxes
    /// The folders pushed in the iPhone's Mail tab.
    var mailPath: [MailScope] = []
    /// The open thread.
    var threadSelection: Int64?
    /// Bumped when mail changed (a sync, a message marked read), so thread
    /// lists reload.
    private(set) var mailVersion = 0
    private(set) var isSyncingMail = false
    private(set) var isFindingToDos = false
    private let network = NWPathMonitor()
    /// The device's calendars and reminders lists (one store for the app).
    let eventStore = EKEventStore()
    private var eventStoreObserver: NSObjectProtocol?
    /// `start` ran (a background wake-up opens the engine without it).
    private var started = false

    /// Open the engine (once); the launch and a background wake-up both
    /// start here.
    private func openEngine() async throws -> Engine {
        if let engine { return engine }
        let engine = try await Engine.open(
            dataDir: Self.dataDirectory.path,
            cacheDir: Self.cacheDirectory.path,
            apple: AppleIntelligence()
        )
        self.engine = engine
        try await defaultToAppleIntelligence(engine)
        watchNetwork(engine)
        configureReminders()
        return engine
    }

    /// Hand the engine the reminders list or calendar chosen in Settings,
    /// when it's still there and mnemis may use it; none otherwise.
    func configureReminders() {
        guard let engine else { return }
        if let destination = ReminderDestination.saved,
           ReminderDestination.hasAccess(destination.kind),
           eventStore.calendar(withIdentifier: destination.calendarId) != nil {
            engine.setReminderStore(store: EventKitReminders(store: eventStore, destination: destination))
        } else {
            engine.setReminderStore(store: nil)
        }
        // Ticked off or moved in Reminders or Calendar: in step at once.
        if eventStoreObserver == nil {
            eventStoreObserver = NotificationCenter.default.addObserver(
                forName: .EKEventStoreChanged, object: eventStore, queue: .main
            ) { [weak self] _ in
                Task { @MainActor in
                    await self?.syncReminders()
                    await self?.refresh()
                }
            }
        }
    }

    /// Bring the device's reminders and the to-dos in line, both ways.
    func syncReminders() async {
        guard let engine else { return }
        do {
            let errors = try await engine.syncReminders()
            if !errors.isEmpty {
                print("[reminders] \(errors.joined(separator: "; "))")
            }
        } catch {
            print("[reminders] \(error)")
        }
    }

    /// Remind about `action` on `day` (midnight UTC of the date) at
    /// `minutes` after local midnight.
    func setReminder(_ action: ActionDto, day: Int64, minutes: Int64) async {
        guard let engine else { return }
        do {
            try await engine.promoteToReminder(actionId: action.id, dueAt: day, remindMinutes: minutes)
        } catch {
            report(error)
        }
        await syncReminders()
        await refresh()
    }

    func clearReminder(_ action: ActionDto) async {
        guard let engine else { return }
        do {
            try await engine.clearReminder(actionId: action.id)
        } catch {
            report(error)
        }
        await syncReminders()
        await refresh()
    }

    func start() async {
        guard !started else { return }
        started = true
        do {
            let engine = try await openEngine()
            await refresh()
            // Always a full sync at launch: new mail, then to-dos.
            Task { await sync(userInitiated: false) }
            // The app went active before the engine was open.
            startWatching()
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
            // `-MnemisOpen mnemis://…` follows a deep link once loaded, without
            // the system's "Open in mnemis?" prompt.
            if let link = UserDefaults.standard.string(forKey: "MnemisOpen"), let url = URL(string: link) {
                await refresh()
                open(url)
            }
            // `-MnemisDeleteChat <id>` deletes a chat left over from a run.
            if let doomed = Int64(UserDefaults.standard.string(forKey: "MnemisDeleteChat") ?? "") {
                try? await engine.deleteChat(chatId: doomed)
                await refreshChats()
                print("[MnemisAsk] deleted chat \(doomed)")
            }
            // `-MnemisEval 2026-07-20:2026-08-21:50` scores extraction with
            // the current LLM over the mail of those dates that actions came
            // from, plus 50 other messages, writing nothing. A fourth part,
            // `staged`, uses the per-message staged turns.
            if let spec = UserDefaults.standard.string(forKey: "MnemisEval") {
                let parts = spec.split(separator: ":").map(String.init)
                let day = Date.ISO8601FormatStyle().year().month().day()
                // A long run must not let the device lock: iOS kills a
                // backgrounded app that keeps the model busy.
                UIApplication.shared.isIdleTimerDisabled = true
                defer { UIApplication.shared.isIdleTimerDisabled = false }
                do {
                    guard parts.count >= 3, let others = Int64(parts[2]) else {
                        throw MnemisError.Failed(message: "expected from:to:others[:staged]")
                    }
                    let report = try await engine.evaluateExtraction(
                        from: Int64(try day.parse(parts[0]).timeIntervalSince1970),
                        to: Int64(try day.parse(parts[1]).timeIntervalSince1970),
                        others: others,
                        staged: parts.count > 3 && parts[3] == "staged"
                    )
                    print("[MnemisEval] \(report)")
                } catch {
                    print("[MnemisEval] failed: \(error)")
                }
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
            resolved = try await engine.listResolvedActions()
            sources = try await engine.listSources()
            chats = try await engine.listChats(includeArchived: false)
            let llm = try await engine.llmConfig()
            needsSetup = sources.isEmpty || llm.chatModel.isEmpty
            mailboxes = try await engine.mailboxes()
            let words = try await engine.userProfile().displayName.split(whereSeparator: \.isWhitespace)
            initials = [words.first, words.count > 1 ? words.last : nil]
                .compactMap { $0?.first.map { String($0).uppercased() } }
                .joined()
        } catch {
            report(error)
        }
        mailVersion += 1
    }

    /// Mail only: new mail, flags and removals in every folder, without
    /// looking for to-dos, so it never waits on the model. Errors show when
    /// the user asked for it (pull to refresh); the launch sync only logs
    /// them.
    func syncMail(userInitiated: Bool = false) async {
        guard let engine, !isSyncingMail else { return }
        isSyncingMail = true
        defer { isSyncingMail = false }
        let finishing = FinishInBackground("mail sync")
        defer { finishing.end() }
        do {
            let errors = try await engine.syncMail()
            if !errors.isEmpty {
                print("[mail] sync: \(errors.joined(separator: "; "))")
                if userInitiated {
                    errorMessage = errors.joined(separator: "\n")
                }
            }
            mailboxes = try await engine.mailboxes()
        } catch {
            report(error)
        }
        mailVersion += 1
    }

    /// Pull to refresh on mail: the folder on screen now, then every
    /// folder and to-dos in the background.
    func refreshMail() async {
        if let engine, !isSyncingMail {
            isSyncingMail = true
            let finishing = FinishInBackground("mail refresh")
            do {
                let errors = try await engine.syncScope(scope: mailScope)
                if !errors.isEmpty {
                    errorMessage = errors.joined(separator: "\n")
                }
                mailboxes = try await engine.mailboxes()
            } catch {
                report(error)
            }
            finishing.end()
            isSyncingMail = false
            mailVersion += 1
        }
        Task {
            await syncMail()
            await findToDos(userInitiated: false)
        }
    }

    /// While the app is in front, the mail servers push changes; the
    /// engine syncs the folders they touch and says so here.
    func startWatching() {
        guard let engine else { return }
        let relay = MailChangeRelay(self)
        Task {
            do {
                try await engine.startWatching(listener: relay)
            } catch {
                print("[mail] push unavailable: \(error)")
            }
        }
    }

    func stopWatching() {
        engine?.stopWatching()
    }

    /// Pushed mail synced: show it, and look for to-dos in it.
    func mailChanged(toDos: Bool) async {
        guard let engine else { return }
        do {
            mailboxes = try await engine.mailboxes()
        } catch {
            report(error)
        }
        mailVersion += 1
        if toDos {
            await findToDos(userInitiated: false)
        }
    }

    /// Look for to-dos in mail synced but not yet read by the model, then
    /// sync the calendar. Skipped without a model.
    func findToDos(userInitiated: Bool) async {
        guard let engine, !isFindingToDos else { return }
        isFindingToDos = true
        defer { isFindingToDos = false }
        let finishing = FinishInBackground("finding to-dos")
        defer { finishing.end() }
        do {
            let outcome = try await engine.findToDos()
            lastSync = outcome
            if !outcome.errors.isEmpty {
                print("[sync] \(outcome.errors.joined(separator: "; "))")
                if userInitiated {
                    errorMessage = outcome.errors.joined(separator: "\n")
                }
            }
        } catch {
            print("[sync] to-dos: \(error)")
            if userInitiated { report(error) }
        }
        await syncReminders()
        await refresh()
    }

    /// Marks messages read or unread here at once; the server hears in the
    /// background.
    func markSeen(_ ids: [Int64], seen: Bool) async {
        guard let engine else { return }
        do {
            try await engine.markSeen(messageIds: ids, seen: seen)
            mailboxes = try await engine.mailboxes()
        } catch {
            report(error)
        }
        mailVersion += 1
    }

    static let backgroundSyncTask = "br.eti.noronha.mnemis.sync"

    /// Ask iOS to wake the app for a sync in about 15 minutes; iOS decides
    /// when, if at all.
    nonisolated static func scheduleBackgroundSync() {
        let request = BGAppRefreshTaskRequest(identifier: backgroundSyncTask)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        do {
            try BGTaskScheduler.shared.submit(request)
            print("[sync] background sync requested")
        } catch {
            print("[sync] background sync not scheduled: \(error)")
        }
    }

    /// A background wake-up: mail always, and to-dos when the model can run
    /// in the background (Claude or omlx; the on-device model stalls there,
    /// so it waits for the next launch). iOS gives this about 30 seconds.
    func backgroundSync() async {
        Self.scheduleBackgroundSync()
        print("[sync] background sync starting")
        do {
            let engine = try await openEngine()
            let llm = try await engine.llmConfig()
            if llm.provider == .appleIntelligence || llm.chatModel.isEmpty {
                await syncMail()
            } else {
                await sync(userInitiated: false)
            }
            print("[sync] background sync done")
        } catch {
            print("[sync] background sync failed: \(error)")
        }
    }

    /// Ask the agent to turn an email into a to-do; true once it has.
    func toDo(fromMessage id: Int64) async -> Bool {
        guard let engine else { return false }
        do {
            _ = try await engine.toDoFromMessage(messageId: id)
            await refresh()
            return true
        } catch {
            report(error)
            return false
        }
    }

    /// Fetching mail ahead only off metered and Low Data networks.
    private func watchNetwork(_ engine: Engine) {
        // Off until the first path update says otherwise, so a launch on
        // cellular doesn't fetch ahead before the monitor has spoken.
        engine.setPrefetchAllowed(allowed: false)
        network.pathUpdateHandler = { path in
            engine.setPrefetchAllowed(allowed: !path.isExpensive && !path.isConstrained)
        }
        network.start(queue: .global(qos: .utility))
    }

    /// New mail, shown as soon as it's in, then to-dos. Errors show when the
    /// user asked for it (Sync); the launch and background syncs only log
    /// them, as the desktop's scheduled ones do.
    func sync(userInitiated: Bool = true) async {
        guard engine != nil, !isSyncing else { return }
        isSyncing = true
        defer { isSyncing = false }
        await syncMail(userInitiated: userInitiated)
        await refresh()
        await findToDos(userInitiated: userInitiated)
    }

    func setStatus(_ action: ActionDto, to status: ActionStatus) async {
        guard let engine else { return }
        do {
            try await engine.setActionStatus(actionId: action.id, status: status, dismissedReason: nil)
        } catch {
            report(error)
        }
        // A resolved to-do leaves the list, so its view closes.
        if [.done, .cancelled, .dismissed].contains(status), actionSelection == action.id {
            actionSelection = nil
        }
        // A done to-do's reminder is ticked off now, not at the next sync.
        await syncReminders()
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
        await talkAbout(kind: "action", id: action.id)
    }

    /// The same for an email.
    func talkAbout(messageId: Int64) async {
        await talkAbout(kind: "message", id: messageId)
    }

    private func talkAbout(kind: String, id entity: Int64) async {
        guard let engine else { return }
        do {
            let id: Int64
            if let existing = try await engine.findSeededChat(seededFromKind: kind, seededFromId: entity) {
                id = existing
            } else {
                id = try await engine.createChat(seededFromKind: kind, seededFromId: entity)
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

    /// Open to-dos, without warnings.
    var todos: [ActionDto] { actions.filter { $0.status != .suspicious } }

    /// Messages from impostor-looking senders, held back as warnings.
    var warnings: [ActionDto] { actions.filter { $0.status == .suspicious } }

    /// Take a warning as a real to-do.
    func accept(_ warning: ActionDto) async {
        await setStatus(warning, to: .pending)
    }

    /// Drop a warning. Unlike dismissing a to-do, there's no rule to learn.
    func discard(_ warning: ActionDto) async {
        await setStatus(warning, to: .dismissed)
        if actionSelection == warning.id { actionSelection = nil }
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

    /// Start a mail, prefilled from `messageId` for a reply or forward, and
    /// open the composer on it.
    func compose(_ kind: DraftKind, messageId: Int64? = nil) async {
        guard let engine else { return }
        do {
            let id = try await engine.startDraft(kind: kind, messageId: messageId)
            composing = try await engine.getDraft(draftId: id)
        } catch {
            report(error)
        }
    }

    /// Open the composer on a draft, such as one the assistant wrote.
    func openDraft(_ id: Int64) async {
        guard let engine else { return }
        do {
            composing = try await engine.getDraft(draftId: id)
        } catch {
            report(error)
        }
    }

    func draftChanged() {
        draftsVersion += 1
    }

    /// Deep links: `mnemis://action/<id>` and `mnemis://chat/<id>` open that
    /// action or chat, `mnemis://draft/<id>` the composer on that draft,
    /// `mnemis://mail/<folder id>` a folder (0: All Inboxes) and
    /// `mnemis://thread/<id>` a thread in All Inboxes (for notifications,
    /// and for driving the app in tests).
    func open(_ url: URL) {
        guard url.scheme == "mnemis", let id = Int64(url.lastPathComponent) else { return }
        switch url.host() {
        case "action":
            selectedTab = .actions
            actionSelection = id
        case "chat":
            selectedTab = .chat
            chatSelection = id
        case "draft":
            Task { await openDraft(id) }
        case "mail":
            showMail(id == 0 ? .allInboxes : .mailbox(channelId: id))
        case "thread":
            showMail(.allInboxes)
            threadSelection = id
        default:
            break
        }
    }

    /// Open a folder in Mail, on either layout.
    func showMail(_ scope: MailScope) {
        selectedTab = .mail
        mailScope = scope
        mailPath = [scope]
        threadSelection = nil
    }

    /// Show a to-do, from wherever it was mentioned.
    func showAction(_ id: Int64) {
        selectedTab = .actions
        actionSelection = id
    }

    func action(_ id: ActionDto.ID?) -> ActionDto? {
        guard let id else { return nil }
        return actions.first { $0.id == id } ?? resolved.first { $0.id == id }
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
        URL.applicationSupportDirectory.appending(path: dataName, directoryHint: .isDirectory)
    }

    /// Where the mail cache lives: everything in it can be fetched again.
    static var cacheDirectory: URL {
        URL.cachesDirectory.appending(path: dataName, directoryHint: .isDirectory)
    }

    private static var dataName: String {
        var name = "mnemis"
        #if DEBUG
        // `-MnemisDataDir <name>` opens another data directory (say, a copy
        // of a desktop database to evaluate against).
        name = UserDefaults.standard.string(forKey: "MnemisDataDir") ?? name
        #endif
        return name
    }
}

/// Hands the engine's push notices to the model on the main actor.
final class MailChangeRelay: MailListener, @unchecked Sendable {
    private weak var model: AppModel?

    init(_ model: AppModel) {
        self.model = model
    }

    func mailChanged(toDos: Bool) {
        Task { @MainActor [weak model] in
            await model?.mailChanged(toDos: toDos)
        }
    }
}
