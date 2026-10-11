import AuthenticationServices
import SwiftUI

/// Add a mail account. Google signs in through its own page (OAuth);
/// providers that take an app-specific password over IMAP get a preset;
/// anything else is "Other".
struct AddAccountView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    enum Preset: String, CaseIterable, Identifiable {
        case fastmail = "Fastmail"
        case google = "Google"
        case icloud = "iCloud"
        case other = "Other"

        var id: Self { self }

        var server: String {
            switch self {
            case .fastmail: "imap.fastmail.com"
            case .google: "imap.gmail.com"
            case .icloud: "imap.mail.me.com"
            case .other: ""
            }
        }

        var passwordHelp: String {
            switch self {
            case .fastmail:
                "Use an app password from Fastmail's Settings → Privacy & Security, with IMAP access."
            case .google:
                "Sign in on Google's page; mnemis never sees your password."
            case .icloud:
                "Use an app-specific password from account.apple.com → Sign-In and Security."
            case .other:
                "mnemis connects over IMAP with TLS."
            }
        }

        var defaultName: String {
            switch self {
            case .google: "Gmail"
            case .other: "Mail"
            default: rawValue
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
    @State private var signIn = GoogleSignIn()

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
            if preset == .google {
                Section {
                    Button {
                        Task { await signInWithGoogle() }
                    } label: {
                        if adding {
                            ProgressView()
                        } else {
                            Text("Sign in with Google")
                        }
                    }
                    .disabled(adding)
                } footer: {
                    Text(preset.passwordHelp)
                }
                Section {
                    TextField("Name", text: $name, prompt: Text(preset.defaultName))
                }
            } else {
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
                    TextField("Name", text: $name, prompt: Text(preset.defaultName))
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
        }
        .navigationTitle("Add account")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            if preset != .google {
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
    }

    private var canAdd: Bool {
        !username.isEmpty && !password.isEmpty && !server.isEmpty && UInt16(port) != nil
    }

    private var displayName: String {
        name.isEmpty ? preset.defaultName : name
    }

    private func add() async {
        guard let engine = model.engine, let port = UInt16(port) else { return }
        adding = true
        defer { adding = false }
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

    /// Google's page in the system's sign-in sheet (shares Safari's
    /// session, so a signed-in user just picks the account), then the
    /// engine trades what it returns for the account's tokens.
    private func signInWithGoogle() async {
        guard let engine = model.engine else { return }
        adding = true
        defer { adding = false }
        do {
            let start = try engine.beginGoogleSignIn()
            guard let url = URL(string: start.url) else { return }
            let callback = try await signIn.run(url: url, callbackScheme: start.callbackScheme)
            _ = try await engine.finishGoogleSignIn(
                callbackUrl: callback.absoluteString, name: displayName)
            await model.refresh()
            Task { await model.sync(userInitiated: false) }
            dismiss()
        } catch let error as ASWebAuthenticationSessionError where error.code == .canceledLogin {
            // The user closed the sheet.
        } catch {
            model.report(error)
        }
    }
}

/// Runs one `ASWebAuthenticationSession`, anchored to the key window.
@MainActor
final class GoogleSignIn: NSObject, ASWebAuthenticationPresentationContextProviding {
    private var session: ASWebAuthenticationSession?

    func run(url: URL, callbackScheme: String) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            let session = ASWebAuthenticationSession(
                url: url, callback: .customScheme(callbackScheme)
            ) { callback, error in
                if let callback {
                    continuation.resume(returning: callback)
                } else {
                    continuation.resume(throwing: error ?? ASWebAuthenticationSessionError(.canceledLogin))
                }
            }
            session.presentationContextProvider = self
            self.session = session
            session.start()
        }
    }

    nonisolated func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        MainActor.assumeIsolated {
            UIApplication.shared.connectedScenes
                .compactMap { ($0 as? UIWindowScene)?.keyWindow }
                .first ?? ASPresentationAnchor()
        }
    }
}
