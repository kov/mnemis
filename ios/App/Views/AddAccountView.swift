import SwiftUI

/// Add an IMAP account. Providers that take an app-specific password over IMAP
/// get a preset; anything else is "Other".
struct AddAccountView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    enum Preset: String, CaseIterable, Identifiable {
        case fastmail = "Fastmail"
        case icloud = "iCloud"
        case other = "Other"

        var id: Self { self }

        var server: String {
            switch self {
            case .fastmail: "imap.fastmail.com"
            case .icloud: "imap.mail.me.com"
            case .other: ""
            }
        }

        var passwordHelp: String {
            switch self {
            case .fastmail:
                "Use an app password from Fastmail's Settings → Privacy & Security, with IMAP access."
            case .icloud:
                "Use an app-specific password from account.apple.com → Sign-In and Security."
            case .other:
                "mnemis connects over IMAP with TLS."
            }
        }
    }

    @State private var preset: Preset = .fastmail
    @State private var name = ""
    @State private var server = Preset.fastmail.server
    @State private var port = "993"
    @State private var username = ""
    @State private var password = ""
    @State private var adding = false

    var body: some View {
        Form {
            Section {
                Picker("Provider", selection: $preset) {
                    ForEach(Preset.allCases) { Text($0.rawValue).tag($0) }
                }
                .onChange(of: preset) { _, new in
                    server = new.server
                    port = "993"
                }
            }
            Section {
                TextField("Email address", text: $username)
                    .textContentType(.username)
                    .keyboardType(.emailAddress)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                SecureField("Password", text: $password)
                    .textContentType(.password)
            } footer: {
                Text(preset.passwordHelp)
            }
            Section {
                TextField("Name", text: $name, prompt: Text(preset == .other ? "Mail" : preset.rawValue))
                if preset == .other {
                    TextField("IMAP server", text: $server)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Port", text: $port)
                        .keyboardType(.numberPad)
                }
            }
        }
        .navigationTitle("Add account")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .confirmationAction) {
                if adding {
                    ProgressView()
                } else {
                    Button("Add") { Task { await add() } }
                        .disabled(!canAdd)
                }
            }
        }
    }

    private var canAdd: Bool {
        !username.isEmpty && !password.isEmpty && !server.isEmpty && UInt16(port) != nil
    }

    private func add() async {
        guard let engine = model.engine, let port = UInt16(port) else { return }
        adding = true
        defer { adding = false }
        let displayName = name.isEmpty ? (preset == .other ? "Mail" : preset.rawValue) : name
        do {
            _ = try await engine.addImapSource(
                name: displayName,
                server: server.trimmingCharacters(in: .whitespaces),
                port: port,
                username: username.trimmingCharacters(in: .whitespaces),
                password: password
            )
            await model.refresh()
            dismiss()
        } catch {
            model.report(error)
        }
    }
}
