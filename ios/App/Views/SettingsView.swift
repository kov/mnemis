import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var loaded = false
    @State private var saving = false

    // Profile
    @State private var displayName = ""
    @State private var emails = ""
    @State private var customPrompt: String?

    // Language model
    @State private var provider: LlmProvider = .anthropic
    @State private var chatModel = ""
    @State private var baseUrl = ""
    @State private var bearerToken = ""
    @State private var thinkingLevel: ThinkingLevel = .medium
    @State private var apiKey = ""
    @State private var apiKeySet = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Name", text: $displayName)
                        .textContentType(.name)
                    TextField("Email addresses", text: $emails, axis: .vertical)
                        .textContentType(.emailAddress)
                        .keyboardType(.emailAddress)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("You")
                } footer: {
                    Text("Separate addresses with commas. mnemis uses them to tell what was asked of you.")
                }

                Section {
                    Picker("Provider", selection: $provider) {
                        Text("Claude").tag(LlmProvider.anthropic)
                        Text("OpenAI Responses (omlx, …)").tag(LlmProvider.openaiResponses)
                    }
                    TextField("Model", text: $chatModel)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    switch provider {
                    case .anthropic:
                        SecureField(apiKeySet ? "API key (saved)" : "API key", text: $apiKey)
                            .textContentType(.password)
                    case .openaiResponses:
                        TextField("Server URL (with /v1)", text: $baseUrl)
                            .keyboardType(.URL)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        SecureField("Bearer token (optional)", text: $bearerToken)
                    }
                    Picker("Thinking", selection: $thinkingLevel) {
                        Text("Low").tag(ThinkingLevel.low)
                        Text("Medium").tag(ThinkingLevel.medium)
                        Text("High").tag(ThinkingLevel.high)
                        Text("Extra high").tag(ThinkingLevel.extraHigh)
                    }
                } header: {
                    Text("Language model")
                } footer: {
                    if provider == .anthropic {
                        Text("The key is kept in this device's Keychain. You can fill it from the Passwords app.")
                    }
                }

                Section("Mail accounts") {
                    ForEach(model.sources, id: \.id) { source in
                        NavigationLink { FoldersView(source: source) } label: {
                            SourceRow(source: source)
                        }
                    }
                    .onDelete(perform: deleteSources)
                    NavigationLink("Add account") { AddAccountView() }
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if saving {
                        ProgressView()
                    } else {
                        Button("Save") { Task { await save() } }
                            .disabled(!loaded)
                    }
                }
            }
            .task { await load() }
            .onChange(of: provider) { old, new in
                // Swap the model's placeholder default along with the provider,
                // but never overwrite one the user typed.
                if chatModel.isEmpty || chatModel == Self.defaultModel(old) {
                    chatModel = Self.defaultModel(new)
                }
            }
        }
    }

    static func defaultModel(_ provider: LlmProvider) -> String {
        switch provider {
        case .anthropic: "claude-sonnet-5-5"
        case .openaiResponses: ""
        }
    }

    private func load() async {
        guard !loaded, let engine = model.engine else { return }
        do {
            let profile = try await engine.userProfile()
            displayName = profile.displayName
            customPrompt = profile.customPrompt
            emails = profile.identifiers
                .filter { $0.kind == "email" }
                .map(\.value)
                .joined(separator: ", ")

            let llm = try await engine.llmConfig()
            if llm.chatModel.isEmpty {
                // Nothing saved yet: start from Claude.
                provider = .anthropic
                chatModel = Self.defaultModel(.anthropic)
            } else {
                provider = llm.provider
                chatModel = llm.chatModel
                baseUrl = llm.baseUrl
                bearerToken = llm.bearerToken ?? ""
            }
            thinkingLevel = llm.thinkingLevel
            apiKeySet = llm.anthropicApiKeySet
            loaded = true
        } catch {
            model.report(error)
        }
    }

    private func save() async {
        guard let engine = model.engine else { return }
        saving = true
        defer { saving = false }
        do {
            let identifiers = emails
                .split(separator: ",")
                .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
                .filter { !$0.isEmpty }
                .map { ProfileIdentifier(kind: "email", value: $0) }
            try await engine.saveUserProfile(profile: UserProfileDto(
                displayName: displayName.trimmingCharacters(in: .whitespaces),
                customPrompt: customPrompt,
                identifiers: identifiers
            ))

            let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
            try await engine.saveLlmConfig(cfg: LlmConfigDto(
                provider: provider,
                baseUrl: provider == .openaiResponses ? baseUrl : "",
                chatModel: chatModel.trimmingCharacters(in: .whitespaces),
                bearerToken: provider == .openaiResponses && !bearerToken.isEmpty ? bearerToken : nil,
                thinkingLevel: thinkingLevel,
                anthropicApiKey: key.isEmpty ? nil : key,
                anthropicApiKeySet: apiKeySet,
                configPath: ""
            ))
            dismiss()
        } catch {
            model.report(error)
        }
    }

    private func deleteSources(at offsets: IndexSet) {
        let doomed = offsets.map { model.sources[$0] }
        Task {
            guard let engine = model.engine else { return }
            for source in doomed {
                do {
                    try await engine.deleteSource(sourceId: source.id)
                } catch {
                    model.report(error)
                }
            }
            await model.refresh()
        }
    }
}

private struct SourceRow: View {
    let source: SourceRowDto

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(source.name)
            Group {
                if let error = source.lastError, source.health != .ok {
                    Text(error).foregroundStyle(.red)
                } else if let synced = source.lastSyncedAt {
                    Text("Checked \(Date.fromUnix(synced), format: .relative(presentation: .named))")
                } else {
                    Text("Not checked yet")
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .lineLimit(2)
        }
    }
}
