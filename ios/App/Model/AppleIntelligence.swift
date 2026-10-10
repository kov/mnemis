import Foundation
import FoundationModels

/// The engine's Apple Intelligence transport. Each `respond` is one model
/// turn: the engine sends the whole conversation (Responses-API shapes) and
/// runs the tools itself, so this rebuilds a Foundation Models transcript,
/// generates, and hands back either text or the tool calls the model made —
/// the tools only capture their calls and stop the turn.
final class AppleIntelligence: AppleModel, @unchecked Sendable {
    static let onDevice = "on-device"
    static let privateCloud = "private-cloud-compute"

    /// Private Cloud Compute needs an entitlement Apple grants per team;
    /// without it the framework traps, so it is compiled in only by builds
    /// that carry it (`MNEMIS_PCC`).
    static var privateCloudAvailable: Bool {
        #if MNEMIS_PCC
        PrivateCloudComputeLanguageModel().isAvailable
        #else
        false
        #endif
    }

    static var onDeviceAvailable: Bool {
        SystemLanguageModel.default.isAvailable
    }

    func contextSize(model: String) async throws -> UInt64 {
        do {
            switch model {
            case Self.privateCloud:
                #if MNEMIS_PCC
                return UInt64(try await PrivateCloudComputeLanguageModel().contextSize)
                #else
                throw Self.failure("Private Cloud Compute isn't enabled in this build")
                #endif
            default:
                return UInt64(SystemLanguageModel.default.contextSize)
            }
        } catch let error as MnemisError {
            throw error
        } catch {
            throw Self.failure(Self.describe(error))
        }
    }

    func respond(request: String, sink: TextSink) async throws -> String {
        do {
            let turn = try Turn(json: request)
            switch turn.model {
            case Self.privateCloud:
                #if MNEMIS_PCC
                return try await turn.run(on: PrivateCloudComputeLanguageModel(), sink: sink)
                #else
                throw Self.failure("Private Cloud Compute isn't enabled in this build")
                #endif
            default:
                let model = SystemLanguageModel(guardrails: .permissiveContentTransformations)
                if case .unavailable(let reason) = model.availability {
                    throw Self.failure("Apple Intelligence isn't available: \(reason)")
                }
                return try await turn.run(on: model, sink: sink)
            }
        } catch let error as MnemisError {
            throw error
        } catch {
            throw Self.failure(Self.describe(error))
        }
    }

    private static func failure(_ message: String) -> MnemisError {
        .Failed(message: message)
    }

    /// Word errors so the engine's classifiers recognise them: an over-long
    /// prompt makes it shrink the input, a stall or rate limit makes it retry.
    static func describe(_ error: Error) -> String {
        if let error = error as? LanguageModelSession.ToolCallError {
            return describe(error.underlyingError)
        }
        if let error = error as? LanguageModelError {
            switch error {
            case .contextSizeExceeded(let context):
                return "Prompt too long: \(context.tokenCount) tokens exceeds max context window of \(context.contextSize)"
            case .rateLimited:
                return "Apple Intelligence is temporarily unavailable (rate limited)"
            case .timeout:
                return "Apple Intelligence timed out"
            case .guardrailViolation:
                return "Apple Intelligence declined: the content tripped its safety guardrails"
            case .refusal(let refusal):
                return "Apple Intelligence refused: \(refusal.debugDescription)"
            default:
                return error.localizedDescription
            }
        }
        #if MNEMIS_PCC
        if let error = error as? PrivateCloudComputeLanguageModel.Error {
            switch error {
            case .networkFailure, .serviceUnavailable:
                return "Private Cloud Compute is temporarily unavailable: \(error.localizedDescription)"
            default:
                return error.localizedDescription
            }
        }
        #endif
        return error.localizedDescription
    }
}

/// One engine request, translated.
private struct Turn {
    let model: String
    let entries: [Transcript.Entry]
    let prompt: String
    let tools: [Transcript.ToolDefinition]

    init(json: String) throws {
        guard let request = try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any] else {
            throw JSONSchema.Failure(errorDescription: "malformed model request")
        }
        model = request["model"] as? String ?? AppleIntelligence.onDevice
        tools = try JSONSchema.toolDefinitions(request["tools"] as? [[String: Any]] ?? [])
        let input = request["input"] as? [[String: Any]] ?? []

        // System messages have no place mid-transcript; they join the
        // instructions.
        var instructions = request["instructions"] as? String ?? ""
        for item in input where item["type"] as? String == "message" && item["role"] as? String == "system" {
            instructions += "\n\n" + (item["content"] as? String ?? "")
        }

        var entries: [Transcript.Entry] = [
            .instructions(.init(segments: [Self.text(instructions)], toolDefinitions: tools)),
        ]
        var toolNames: [String: String] = [:]
        var calls: [Transcript.ToolCall] = []
        func flushCalls() {
            if !calls.isEmpty {
                entries.append(.toolCalls(.init(calls)))
                calls = []
            }
        }
        for item in input {
            switch item["type"] as? String {
            case "message":
                flushCalls()
                let content = item["content"] as? String ?? ""
                switch item["role"] as? String {
                case "user": entries.append(.prompt(.init(segments: [Self.text(content)])))
                case "assistant": entries.append(.response(.init(segments: [Self.text(content)])))
                default: break
                }
            case "function_call":
                let id = item["call_id"] as? String ?? UUID().uuidString
                let name = item["name"] as? String ?? ""
                toolNames[id] = name
                let arguments = (try? GeneratedContent(json: item["arguments"] as? String ?? "{}"))
                    ?? GeneratedContent(properties: [:])
                calls.append(Transcript.ToolCall(id: id, toolName: name, arguments: arguments))
            case "function_call_output":
                flushCalls()
                let id = item["call_id"] as? String ?? ""
                entries.append(.toolOutput(.init(
                    id: id,
                    toolName: toolNames[id] ?? "",
                    segments: [Self.text(item["output"] as? String ?? "")]
                )))
            default:
                break
            }
        }
        flushCalls()

        // A trailing user message is this turn's prompt. After tool output
        // there is none: an empty prompt continues from the results.
        if case .prompt(let last) = entries.last {
            entries.removeLast()
            prompt = last.segments.compactMap { segment -> String? in
                if case .text(let text) = segment { text.content } else { nil }
            }.joined()
        } else {
            prompt = ""
        }
        self.entries = entries
    }

    private static func text(_ content: String) -> Transcript.Segment {
        .text(.init(content: content))
    }

    func run(on model: some LanguageModel, sink: TextSink) async throws -> String {
        let capture = CallCapture()
        let session = LanguageModelSession(
            model: model,
            tools: tools.map { CapturingTool(definition: $0, capture: capture) },
            transcript: Transcript(entries: entries)
        )
        capture.session = session

        var text = ""
        do {
            for try await snapshot in session.streamResponse(to: prompt) {
                let full = snapshot.content
                if full.hasPrefix(text) {
                    let delta = String(full.dropFirst(text.count))
                    if !delta.isEmpty { sink.push(text: delta) }
                }
                text = full
            }
        } catch let error as LanguageModelSession.ToolCallError where error.underlyingError is CapturedCall {
            // The model asked for tools; the engine runs them.
        }

        var output: [[String: Any]] = []
        if !text.isEmpty {
            output.append(["type": "message", "content": [["type": "output_text", "text": text]]])
        }
        for call in capture.calls {
            output.append([
                "type": "function_call",
                "call_id": call.id,
                "name": call.toolName,
                "arguments": call.arguments.jsonString,
            ])
        }
        let reply: [String: Any] = ["id": "", "status": "completed", "output": output]
        return String(decoding: try JSONSerialization.data(withJSONObject: reply), as: UTF8.self)
    }
}

private struct CapturedCall: Error {}

/// The tool calls a turn ended on. The session lists the whole batch in its
/// transcript before running any tool, so the first call captures them all.
private final class CallCapture: @unchecked Sendable {
    weak var session: LanguageModelSession?
    private let lock = NSLock()
    private var captured: [Transcript.ToolCall]?

    var calls: [Transcript.ToolCall] {
        lock.withLock { captured ?? [] }
    }

    func record(_ call: Transcript.ToolCall) {
        let tail = session?.transcript.last
        lock.withLock {
            guard captured == nil else { return }
            if case .toolCalls(let batch) = tail {
                captured = Array(batch)
            } else {
                captured = [call]
            }
        }
    }
}

/// Stands in for an engine tool: records the call and stops the turn.
private struct CapturingTool: Tool {
    typealias Arguments = GeneratedContent
    typealias Output = String

    let name: String
    let description: String
    let parameters: GenerationSchema
    let capture: CallCapture

    init(definition: Transcript.ToolDefinition, capture: CallCapture) {
        name = definition.name
        description = definition.description
        parameters = definition.parameters
        self.capture = capture
    }

    func call(arguments: GeneratedContent) async throws -> String {
        capture.record(Transcript.ToolCall(id: UUID().uuidString, toolName: name, arguments: arguments))
        throw CapturedCall()
    }
}
