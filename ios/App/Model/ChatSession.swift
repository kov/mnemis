import Foundation
import Observation

/// One open conversation. The engine persists every turn before it streams the
/// matching event, so the transcript is always reloaded from the database; the
/// events only drive what's live in between — streamed text, "thinking",
/// "condensing".
@MainActor
@Observable
final class ChatSession {
    let chatId: Int64
    private(set) var turns: [ChatTurnDto] = []
    private(set) var seedLabel: String?
    /// Assistant text streaming in, not yet persisted as a turn.
    private(set) var liveText = ""
    /// The user's message while it's being sent, until it shows up in `turns`.
    private(set) var pendingUserText: String?
    private(set) var sending = false
    private(set) var compacting = false

    init(chatId: Int64) {
        self.chatId = chatId
    }

    func load(_ engine: Engine, model: AppModel) async {
        do {
            seedLabel = try await engine.chatSeedLabel(chatId: chatId)
            turns = try await engine.chatTurns(chatId: chatId)
        } catch {
            model.report(error)
        }
    }

    func send(_ text: String, engine: Engine, model: AppModel) async {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !sending else { return }
        sending = true
        pendingUserText = text
        liveText = ""

        // The engine calls the listener from its own threads; the stream keeps
        // the events in order and hands them to the main actor.
        let (events, continuation) = AsyncStream.makeStream(of: ChatEvent.self)
        let relay = ChatEventRelay(continuation)
        let consumer = Task { @MainActor in
            var failed = false
            for await event in events {
                if case let .error(message) = event {
                    failed = true
                    model.errorMessage = message
                }
                await handle(event, engine: engine, model: model)
            }
            return failed
        }

        var thrown: Error?
        do {
            try await engine.sendChatMessage(chatId: chatId, text: text, listener: relay)
        } catch {
            thrown = error
        }
        continuation.finish()
        let reportedByEvent = await consumer.value
        // The engine reports most failures as an `Error` event too; only
        // surface the thrown error when it didn't (e.g. no LLM configured).
        if let thrown, !reportedByEvent {
            model.report(thrown)
        }

        await reload(engine, model: model)
        liveText = ""
        pendingUserText = nil
        compacting = false
        sending = false
        await model.refreshChats()
    }

    func stop(_ engine: Engine) {
        engine.cancelChatMessage(chatId: chatId)
    }

    private func handle(_ event: ChatEvent, engine: Engine, model: AppModel) async {
        switch event {
        case let .delta(text):
            compacting = false
            liveText += text
        case .compacting:
            compacting = true
        case .reasoning, .assistantMessage, .toolCall, .toolResult:
            // Persisted already: show it from the transcript, and drop the
            // streamed copy so nothing renders twice.
            compacting = false
            await reload(engine, model: model)
            liveText = ""
        case .done, .error:
            break
        }
    }

    private func reload(_ engine: Engine, model: AppModel) async {
        do {
            turns = try await engine.chatTurns(chatId: chatId)
            if turns.contains(where: { $0.role == "user" && $0.content == pendingUserText }) {
                pendingUserText = nil
            }
        } catch {
            model.report(error)
        }
    }
}

/// Bridges the engine's callback into an `AsyncStream`.
final class ChatEventRelay: ChatListener, @unchecked Sendable {
    private let continuation: AsyncStream<ChatEvent>.Continuation

    init(_ continuation: AsyncStream<ChatEvent>.Continuation) {
        self.continuation = continuation
    }

    func onEvent(event: ChatEvent) {
        continuation.yield(event)
    }
}
