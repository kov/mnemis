import Foundation
import FoundationModels

/// Turns the engine's tool definitions (OpenAI-style JSON Schema) into
/// Foundation Models tool definitions. Covers what the engine's tools use:
/// objects, strings (optionally an enum), integers, numbers, booleans, arrays,
/// and `["string", "null"]` for an optional value.
enum JSONSchema {
    struct Failure: LocalizedError {
        let errorDescription: String?
    }

    /// `[{"name", "description", "parameters"}, …]` → tool definitions.
    static func toolDefinitions(_ tools: [[String: Any]]) throws -> [Transcript.ToolDefinition] {
        try tools.map { tool in
            let name = tool["name"] as? String ?? ""
            let parameters = tool["parameters"] as? [String: Any] ?? ["type": "object"]
            let root = try schema(parameters, name: name)
            return Transcript.ToolDefinition(
                name: name,
                description: tool["description"] as? String ?? "",
                parameters: try GenerationSchema(root: root, dependencies: [])
            )
        }
    }

    static func schema(_ json: [String: Any], name: String) throws -> DynamicGenerationSchema {
        let description = json["description"] as? String
        if let choices = json["enum"] as? [String] {
            return DynamicGenerationSchema(name: name, description: description, anyOf: choices)
        }
        switch primaryType(json["type"]) {
        case "object":
            let properties = json["properties"] as? [String: Any] ?? [:]
            let required = Set(json["required"] as? [String] ?? [])
            let props = try properties.keys.sorted().map { key in
                let child = properties[key] as? [String: Any] ?? [:]
                return DynamicGenerationSchema.Property(
                    name: key,
                    description: child["description"] as? String,
                    schema: try schema(child, name: "\(name)_\(key)"),
                    isOptional: !required.contains(key) || isNullable(child["type"])
                )
            }
            return DynamicGenerationSchema(name: name, description: description, properties: props)
        case "array":
            let items = json["items"] as? [String: Any] ?? ["type": "string"]
            return DynamicGenerationSchema(
                arrayOf: try schema(items, name: "\(name)_item"),
                minimumElements: json["minItems"] as? Int,
                maximumElements: json["maxItems"] as? Int
            )
        case "integer":
            return DynamicGenerationSchema(type: Int.self)
        case "number":
            return DynamicGenerationSchema(type: Double.self)
        case "boolean":
            return DynamicGenerationSchema(type: Bool.self)
        case "string", nil:
            return DynamicGenerationSchema(type: String.self)
        case let other?:
            throw Failure(errorDescription: "unsupported JSON Schema type \(other) in \(name)")
        }
    }

    /// The non-null member of `type`, which may be a string or an array.
    private static func primaryType(_ type: Any?) -> String? {
        if let single = type as? String { return single }
        return (type as? [String])?.first { $0 != "null" }
    }

    private static func isNullable(_ type: Any?) -> Bool {
        (type as? [String])?.contains("null") ?? false
    }
}
