import SwiftUI

/// One mail account: which folders mnemis reads, and how it sends.
struct AccountView: View {
    @Environment(AppModel.self) private var model
    let source: SourceRowDto

    @State private var smtp: SmtpSettingsDto?
    @State private var password = ""
    @State private var saving = false
    @State private var saved = false

    var body: some View {
        Form {
            Section {
                NavigationLink("Folders") { FoldersView(source: source) }
            }
            if let smtp = Binding($smtp) {
                Section {
                    TextField("Name", text: smtp.fromName)
                        .textContentType(.name)
                    TextField("Address", text: smtp.fromAddress)
                        .keyboardType(.emailAddress)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("Sender")
                } footer: {
                    Text("How your mail is signed when you send from this account.")
                }
                Section {
                    TextField("Server", text: smtp.server)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Port", value: smtp.port, format: .number.grouping(.never))
                        .keyboardType(.numberPad)
                    Toggle("STARTTLS", isOn: smtp.starttls)
                    TextField("Username", text: smtp.username)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    SecureField(
                        smtp.wrappedValue.ownPassword ? "Password (saved)" : "Password (same as for reading)",
                        text: $password
                    )
                    .textContentType(.password)
                    Toggle("Save a copy to Sent", isOn: smtp.saveSent)
                } header: {
                    Text("Outgoing server")
                } footer: {
                    if smtp.wrappedValue.saved {
                        Text("Turn on \"Save a copy to Sent\" only if the server doesn't already keep the mail you send. Gmail does, so a copy there would show up twice.")
                    } else {
                        Text("Guessed from the account's incoming server; check them before your first send. Turn on \"Save a copy to Sent\" only if the server doesn't already keep the mail you send.")
                    }
                }
                Section {
                    Button {
                        Task { await save() }
                    } label: {
                        if saving {
                            ProgressView()
                        } else {
                            Text(saved ? "Saved" : "Save")
                        }
                    }
                    .disabled(saving)
                }
            }
        }
        .navigationTitle(source.name)
        .navigationBarTitleDisplayMode(.inline)
        .task { await load() }
        .onChange(of: smtp?.fromAddress) { saved = false }
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            smtp = try await engine.smtpSettings(sourceId: source.id)
        } catch {
            model.report(error)
        }
    }

    private func save() async {
        guard let engine = model.engine, let smtp else { return }
        saving = true
        defer { saving = false }
        do {
            try await engine.saveSmtpSettings(settings: smtp, password: password.isEmpty ? nil : password)
            password = ""
            self.smtp = try await engine.smtpSettings(sourceId: source.id)
            saved = true
        } catch {
            model.report(error)
        }
    }
}
