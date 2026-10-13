import SwiftUI

/// First launch with no mail account: welcome, then an account, its folders,
/// the user (name from their sent mail, about, interests, language), the
/// model, reminders, and the first sync.
struct OnboardingView: View {
    @State private var path: [Step] = []

    enum Step: Hashable {
        case account, folders, about, model, reminders, done
    }

    var body: some View {
        NavigationStack(path: $path) {
            WelcomeStep { path.append(.account) }
                .navigationDestination(for: Step.self) { step in
                    switch step {
                    case .account: AccountStep { path.append(.folders) }
                    case .folders: FoldersStep { path.append(.about) }
                    case .about: AboutStep { path.append(.model) }
                    case .model: ModelStep { path.append(.reminders) }
                    case .reminders: RemindersStep { path.append(.done) }
                    case .done: DoneStep()
                    }
                }
        }
        .interactiveDismissDisabled()
    }
}

/// The button at the foot of every step.
private struct ContinueButton: View {
    var title = "Continue"
    var enabled = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.headline)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 6)
        }
        .buttonStyle(.borderedProminent)
        .disabled(!enabled)
        .padding(.horizontal)
        .padding(.bottom, 8)
    }
}

private struct WelcomeStep: View {
    let next: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 28) {
            Spacer()
            Text("Welcome to mnemis")
                .font(.largeTitle.bold())
            VStack(alignment: .leading, spacing: 20) {
                point("checklist", "Finds what your mail asks of you, and keeps it as to-dos with reminders.")
                point("sun.horizon", "A briefing of what came in and what is worth reading.")
                point("bubble.left.and.text.bubble.right",
                      "An assistant that knows your mail, and can keep watch for what you're waiting on.")
            }
            .font(.body)
            Spacer()
            Spacer()
        }
        .padding(.horizontal, 28)
        .safeAreaInset(edge: .bottom) {
            ContinueButton(title: "Get started", action: next)
        }
    }

    /// One line of what mnemis does, its icon in a column of its own.
    private func point(_ icon: String, _ text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 14) {
            Image(systemName: icon)
                .foregroundStyle(.tint)
                .frame(width: 32)
            Text(text)
        }
    }
}

private struct AccountStep: View {
    @Environment(AppModel.self) private var model
    let next: () -> Void

    var body: some View {
        Form {
            Section {
                ForEach(model.sources, id: \.id) { source in
                    Label(source.name, systemImage: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                }
                NavigationLink {
                    AddAccountView()
                } label: {
                    Label(model.sources.isEmpty ? "Add an account" : "Add another account",
                          systemImage: "plus")
                }
            } footer: {
                Text("mnemis reads your mail over IMAP. Google signs in on its own page; Fastmail and iCloud take an app password.")
            }
        }
        .navigationTitle("Your mail")
        .safeAreaInset(edge: .bottom) {
            ContinueButton(enabled: !model.sources.isEmpty) {
                // Fetch the folders and what you sent, for your name.
                Task { await model.syncMail() }
                next()
            }
        }
    }
}

private struct FoldersStep: View {
    @Environment(AppModel.self) private var model
    let next: () -> Void

    var body: some View {
        Group {
            if model.sources.count == 1, let source = model.sources.first {
                FoldersView(source: source)
                    .navigationTitle("Folders")
            } else {
                List(model.sources, id: \.id) { source in
                    NavigationLink(source.name) { FoldersView(source: source) }
                }
                .navigationTitle("Folders")
            }
        }
        .safeAreaInset(edge: .bottom) {
            VStack(spacing: 8) {
                Text("To-dos and the briefing come from the folders that are on. The inbox is enough to start.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal)
                ContinueButton(action: next)
            }
            .background(.bar)
        }
    }
}

private struct AboutStep: View {
    @Environment(AppModel.self) private var model
    let next: () -> Void

    @State private var name = ""
    /// The name found in the user's sent mail; replaced by a better find
    /// until the user types their own.
    @State private var suggested: String?
    @State private var about = ""
    @State private var interests = ""
    @State private var language: String? = LanguagePicker.deviceLanguage
    @State private var identifiers: [ProfileIdentifier] = []
    @State private var saving = false

    var body: some View {
        Form {
            Section {
                TextField("Your name", text: $name)
                    .textContentType(.name)
            } header: {
                Text("Name")
            } footer: {
                if !name.isEmpty && name == suggested {
                    Text("From the mail you've sent.")
                } else if name.isEmpty {
                    Text("Looking for it in the mail you've sent…")
                }
            }
            Section {
                TextField("e.g. I work at Igalia; Ana is my manager; my bank is Itaú",
                          text: $about, axis: .vertical)
                    .lineLimit(2...6)
            } header: {
                Text("About you")
            } footer: {
                Text("Helps tell what mail is for you, and real senders from imitations.")
            }
            Section {
                TextField("e.g. anything from my kids' school; not newsletters",
                          text: $interests, axis: .vertical)
                    .lineLimit(2...6)
            } header: {
                Text("What you want to hear about")
            } footer: {
                Text("The briefing judges your mail by this. You can also tell the assistant later.")
            }
            Section {
                LanguagePicker(language: $language)
            } footer: {
                Text("To-dos, summaries and the briefing are written in this language.")
            }
        }
        .navigationTitle("About you")
        .safeAreaInset(edge: .bottom) {
            ContinueButton(enabled: !name.trimmingCharacters(in: .whitespaces).isEmpty && !saving) {
                Task { await save() }
            }
        }
        .task { await loadProfile() }
        .task(id: model.mailVersion) { await suggest() }
    }

    private func loadProfile() async {
        guard let engine = model.engine else { return }
        do {
            let profile = try await engine.userProfile()
            identifiers = profile.identifiers
            if name.isEmpty {
                name = profile.displayName
            }
            about = profile.customPrompt ?? ""
            interests = try await engine.interests() ?? ""
            if let saved = try await engine.language() {
                language = saved
            }
        } catch {
            model.report(error)
        }
    }

    /// The name from the mail the user sent, until they type their own.
    private func suggest() async {
        guard let engine = model.engine, name.isEmpty || name == suggested,
              let found = try? await engine.suggestedName()
        else { return }
        if name.isEmpty || name == suggested {
            name = found
        }
        suggested = found
    }

    private func save() async {
        guard let engine = model.engine else { return }
        saving = true
        defer { saving = false }
        do {
            let about = about.trimmingCharacters(in: .whitespacesAndNewlines)
            try await engine.saveUserProfile(profile: UserProfileDto(
                displayName: name.trimmingCharacters(in: .whitespaces),
                customPrompt: about.isEmpty ? nil : about,
                identifiers: identifiers
            ))
            try await engine.saveInterests(interests: interests)
            try await engine.saveLanguage(language: language)
            await model.refresh()
            next()
        } catch {
            model.report(error)
        }
    }
}

private struct ModelStep: View {
    @Environment(AppModel.self) private var model
    let next: () -> Void

    @State private var provider: LlmProvider =
        AppleIntelligence.onDeviceAvailable ? .appleIntelligence : .anthropic
    @State private var apiKey = ""
    @State private var claudeModel = SettingsView.defaultModel(.anthropic)
    @State private var serverUrl = ""
    @State private var serverModel = ""
    @State private var serverToken = ""
    @State private var saving = false

    var body: some View {
        Form {
            Section {
                choice(.appleIntelligence, title: "Apple Intelligence",
                       detail: AppleIntelligence.onDeviceAvailable
                           ? "On this device: your mail never leaves it."
                           : "Turn on Apple Intelligence in Settings to use it.")
                    .disabled(!AppleIntelligence.onDeviceAvailable)
                choice(.anthropic, title: "Claude",
                       detail: "Anthropic's models, with your API key. Better at to-dos and briefings; mail is sent to Anthropic.")
                choice(.openaiResponses, title: "Your own server",
                       detail: "An OpenAI Responses-compatible server, such as omlx.")
            } header: {
                Text("Which model reads your mail")
            }
            switch provider {
            case .appleIntelligence:
                EmptyView()
            case .anthropic:
                Section {
                    SecureField("API key", text: $apiKey)
                        .textContentType(.password)
                    TextField("Model", text: $claudeModel)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } footer: {
                    Text("The key is kept in this device's Keychain. Get one at console.anthropic.com.")
                }
            case .openaiResponses:
                Section {
                    TextField("Server URL (with /v1)", text: $serverUrl)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Model", text: $serverModel)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField("Bearer token (optional)", text: $serverToken)
                }
            }
        }
        .navigationTitle("Language model")
        .safeAreaInset(edge: .bottom) {
            ContinueButton(enabled: canContinue && !saving) {
                Task { await save() }
            }
        }
    }

    private func choice(_ value: LlmProvider, title: String, detail: String) -> some View {
        Button {
            provider = value
        } label: {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).foregroundStyle(.primary)
                    Text(detail).font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if provider == value {
                    Image(systemName: "checkmark").foregroundStyle(.tint)
                }
            }
        }
    }

    private var canContinue: Bool {
        switch provider {
        case .appleIntelligence: AppleIntelligence.onDeviceAvailable
        case .anthropic: !apiKey.trimmingCharacters(in: .whitespaces).isEmpty && !claudeModel.isEmpty
        case .openaiResponses: !serverUrl.isEmpty && !serverModel.isEmpty
        }
    }

    private func save() async {
        guard let engine = model.engine else { return }
        saving = true
        defer { saving = false }
        do {
            let current = try await engine.llmConfig()
            let key = apiKey.trimmingCharacters(in: .whitespacesAndNewlines)
            let chatModel: String = switch provider {
            case .appleIntelligence: AppleIntelligence.onDevice
            case .anthropic: claudeModel.trimmingCharacters(in: .whitespaces)
            case .openaiResponses: serverModel.trimmingCharacters(in: .whitespaces)
            }
            try await engine.saveLlmConfig(cfg: LlmConfigDto(
                provider: provider,
                baseUrl: provider == .openaiResponses ? serverUrl.trimmingCharacters(in: .whitespaces) : "",
                chatModel: chatModel,
                bearerToken: provider == .openaiResponses && !serverToken.isEmpty ? serverToken : nil,
                thinkingLevel: current.thinkingLevel,
                anthropicApiKey: provider == .anthropic && !key.isEmpty ? key : nil,
                anthropicApiKeySet: current.anthropicApiKeySet,
                configPath: ""
            ))
            await model.refresh()
            next()
        } catch {
            model.report(error)
        }
    }
}

private struct RemindersStep: View {
    let next: () -> Void

    var body: some View {
        ReminderSettingsView()
            .navigationTitle("Reminders")
            .safeAreaInset(edge: .bottom) {
                VStack(spacing: 8) {
                    Text("To-dos with a date can appear in Reminders or Calendar, alongside your own.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .padding(.horizontal)
                    ContinueButton(action: next)
                }
                .background(.bar)
            }
    }
}

private struct DoneStep: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            Spacer()
            Image(systemName: "checkmark.circle.fill")
                .font(.system(size: 56))
                .foregroundStyle(.green)
            Text("You're all set")
                .font(.largeTitle.bold())
            Text("mnemis now reads your mail. To-dos appear as it finds them, and the briefing once it has sorted the last week's mail. That first pass takes a while.")
                .foregroundStyle(.secondary)
            Spacer()
            Spacer()
        }
        .padding(.horizontal, 28)
        .navigationBarBackButtonHidden()
        .safeAreaInset(edge: .bottom) {
            ContinueButton(title: "Start") { model.finishOnboarding() }
        }
    }
}
