import QuickLook
import SwiftUI

// The mail reader: folders, a folder's threads, and a thread's messages.
// Read messages start folded, unread ones and the latest open; an unread
// message counts as read once it has been on screen for a moment.

/// Threads per page of a list.
private let threadPage: Int64 = 50

extension MailboxDto {
    /// How deep the folder sits below the top, as its path shows it.
    var depth: Int {
        if path.contains("/") {
            return path.filter { $0 == "/" }.count
        }
        if path.lowercased().hasPrefix("inbox.") {
            return path.filter { $0 == "." }.count
        }
        return 0
    }

    var systemImage: String {
        switch role {
        case .inbox: "tray"
        case .drafts: "doc"
        case .sent: "paperplane"
        case .archive: "archivebox"
        case .junk: "xmark.bin"
        case .trash: "trash"
        case .other: "folder"
        }
    }

    var scope: MailScope { .mailbox(channelId: channelId) }
}

extension AppModel {
    /// The folders grouped by account, in the engine's order.
    var mailAccounts: [(account: String, folders: [MailboxDto])] {
        var out: [(account: String, folders: [MailboxDto])] = []
        for box in mailboxes {
            if let i = out.firstIndex(where: { $0.account == box.account }) {
                out[i].folders.append(box)
            } else {
                out.append((box.account, [box]))
            }
        }
        return out
    }

    var inboxUnread: Int {
        Int(mailboxes.filter { $0.role == .inbox }.reduce(0) { $0 + $1.unread })
    }

    func title(for scope: MailScope) -> String {
        switch scope {
        case .allInboxes:
            return "All Inboxes"
        case let .mailbox(channelId):
            return mailboxes.first { $0.channelId == channelId }?.name ?? "Mail"
        }
    }
}

/// A folder's row: its icon, indented under its parent, with unread count.
struct MailboxLabel: View {
    let box: MailboxDto

    var body: some View {
        Label(box.name, systemImage: box.systemImage)
            .padding(.leading, CGFloat(box.depth) * 14)
            .badge(Int(box.unread))
    }
}

/// iPhone: the Mail tab's root, every folder; picking one pushes its threads.
struct MailboxListView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        List {
            NavigationLink(value: MailScope.allInboxes) {
                Label("All Inboxes", systemImage: "tray.2")
                    .badge(model.inboxUnread)
            }
            ForEach(model.mailAccounts, id: \.account) { account in
                Section(account.account) {
                    ForEach(account.folders, id: \.channelId) { box in
                        NavigationLink(value: box.scope) { MailboxLabel(box: box) }
                    }
                }
            }
        }
        .refreshable { await model.syncMail() }
        .navigationDestination(for: MailScope.self) { scope in
            ThreadListView(scope: scope)
        }
    }
}

/// A folder's threads (or every inbox's), latest first. Nothing opens by
/// itself: opening a thread marks its mail read.
struct ThreadListView: View {
    @Environment(AppModel.self) private var model
    let scope: MailScope

    @State private var threads: [ThreadSummaryDto]?
    @State private var hasMore = false

    var body: some View {
        @Bindable var model = model
        List(selection: $model.threadSelection) {
            ForEach(threads ?? [], id: \.threadId) { thread in
                ThreadRow(thread: thread)
                    .tag(thread.threadId)
                    .onAppear {
                        if hasMore, thread.threadId == threads?.last?.threadId {
                            Task { await loadMore() }
                        }
                    }
            }
        }
        .listStyle(.plain)
        .overlay {
            if threads == nil {
                ProgressView()
            } else if threads?.isEmpty == true {
                ContentUnavailableView(
                    "No mail",
                    systemImage: "tray",
                    description: Text("Nothing here in the last 90 days.")
                )
            }
        }
        .navigationTitle(model.title(for: scope))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                if model.isSyncingMail { ProgressView() }
            }
        }
        .refreshable { await model.syncMail() }
        .task(id: model.mailVersion) { await reload() }
        .navigationDestination(item: $model.threadSelection) { id in
            ThreadView(threadId: id).id(id)
        }
    }

    /// The first page again, or as many as are showing.
    private func reload() async {
        guard let engine = model.engine else { return }
        let limit = max(threadPage, Int64(threads?.count ?? 0))
        do {
            let rows = try await engine.mailThreads(scope: scope, before: nil, limit: limit)
            threads = rows
            hasMore = rows.count >= limit
        } catch {
            threads = threads ?? []
            model.report(error)
        }
    }

    private func loadMore() async {
        guard let engine = model.engine, let last = threads?.last else { return }
        hasMore = false
        do {
            let rows = try await engine.mailThreads(scope: scope, before: last.latestAt, limit: threadPage)
            threads?.append(contentsOf: rows.filter { row in !(threads ?? []).contains { $0.threadId == row.threadId } })
            hasMore = rows.count >= threadPage
        } catch {
            model.report(error)
        }
    }
}

struct ThreadRow: View {
    let thread: ThreadSummaryDto

    private var unread: Bool { thread.unreadCount > 0 }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Circle()
                .fill(unread ? Color.accentColor : .clear)
                .frame(width: 9, height: 9)
                .padding(.top, 5)
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline) {
                    Text(thread.participants.isEmpty ? "Unknown" : thread.participants.joined(separator: ", "))
                        .font(.subheadline.weight(unread ? .semibold : .regular))
                        .lineLimit(1)
                    if thread.messageCount > 1 {
                        Text("\(thread.messageCount)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 4)
                    Text(Date.fromUnix(thread.latestAt), format: .relative(presentation: .named))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Text(thread.subject.isEmpty ? "(no subject)" : thread.subject)
                    .font(.subheadline.weight(unread ? .semibold : .regular))
                    .lineLimit(1)
                Text(thread.snippet)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                if thread.todoCount > 0 || thread.hasAttachments || thread.flagged {
                    HStack(spacing: 6) {
                        if thread.todoCount > 0 {
                            Badge(text: "\(thread.todoCount) to-do", systemImage: "checklist", tint: .blue)
                        }
                        if thread.hasAttachments {
                            Image(systemName: "paperclip").font(.caption).foregroundStyle(.secondary)
                        }
                        if thread.flagged {
                            Image(systemName: "flag.fill").font(.caption).foregroundStyle(.orange)
                        }
                    }
                    .padding(.top, 2)
                }
            }
        }
        .padding(.vertical, 2)
    }
}

/// The open thread: its to-dos, then its messages, oldest first.
struct ThreadView: View {
    @Environment(AppModel.self) private var model
    let threadId: Int64

    @State private var thread: ThreadDto?
    /// Which messages are open; decided once, when the thread first loads
    /// (unread ones and the latest), then only the user changes it.
    @State private var open: Set<Int64>?
    /// Bumped when the thread's missing mail has arrived, so incomplete
    /// messages render again.
    @State private var downloaded = 0

    var body: some View {
        ScrollView {
            if let thread, let open {
                VStack(alignment: .leading, spacing: 12) {
                    Text(thread.subject.isEmpty ? "(no subject)" : thread.subject)
                        .font(.title3.weight(.bold))
                        .fixedSize(horizontal: false, vertical: true)
                    if !thread.todos.isEmpty {
                        VStack(alignment: .leading, spacing: 6) {
                            ForEach(thread.todos, id: \.actionId) { todo in
                                Button {
                                    model.showAction(todo.actionId)
                                } label: {
                                    Label(todo.title, systemImage: "checklist")
                                        .font(.subheadline)
                                        .multilineTextAlignment(.leading)
                                }
                                .buttonStyle(.glass)
                            }
                        }
                    }
                    ForEach(thread.messages, id: \.messageId) { message in
                        MessageCard(
                            message: message,
                            expanded: Binding(
                                get: { open.contains(message.messageId) },
                                set: { on in
                                    if on {
                                        self.open?.insert(message.messageId)
                                    } else {
                                        self.open?.remove(message.messageId)
                                    }
                                }
                            ),
                            downloaded: downloaded
                        )
                    }
                }
                .padding()
                .frame(maxWidth: 860)
                .frame(maxWidth: .infinity)
            }
        }
        .overlay {
            if thread == nil { ProgressView() }
        }
        .background(Color(.systemGroupedBackground))
        .navigationTitle("")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            await load()
            guard let engine = model.engine else { return }
            do {
                if try await engine.fetchThread(threadId: threadId) > 0 {
                    downloaded += 1
                    await load()
                }
            } catch {
                print("[mail] couldn't download thread \(threadId): \(error)")
            }
        }
    }

    private func load() async {
        guard let engine = model.engine else { return }
        do {
            let loaded = try await engine.mailThread(threadId: threadId)
            if open == nil {
                let last = loaded.messages.last?.messageId
                open = Set(loaded.messages.filter { !$0.seen || $0.messageId == last }.map(\.messageId))
            }
            thread = loaded
        } catch {
            model.report(error)
        }
    }
}

private struct MessageCard: View {
    @Environment(AppModel.self) private var model
    let message: ThreadMessageDto
    @Binding var expanded: Bool
    let downloaded: Int

    @State private var seen: Bool?

    private var isSeen: Bool { seen ?? message.seen }

    private var name: String {
        if message.fromMe { return "Me" }
        if let name = message.fromName, !name.trimmingCharacters(in: .whitespaces).isEmpty { return name }
        return message.fromAddress ?? "Unknown"
    }

    var body: some View {
        Card {
            Button {
                withAnimation(.snappy) { expanded.toggle() }
            } label: {
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 6) {
                            Text(name).font(.subheadline.weight(.semibold))
                            if !isSeen {
                                Circle().fill(Color.accentColor).frame(width: 7, height: 7)
                            }
                        }
                        Text(expanded ? (message.fromAddress ?? "") : message.snippet)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Spacer(minLength: 8)
                    Text(Date.fromUnix(message.postedAt), format: .dateTime.day().month().hour().minute())
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .contentShape(.rect)
            }
            .buttonStyle(.plain)

            if expanded {
                if !recipients.isEmpty {
                    Text(recipients)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                MessageBody(messageId: message.messageId, downloaded: downloaded)
                tools
            }
        }
        // Unread and open for a moment: it's been read. Folding it or
        // leaving the thread first cancels this.
        .task(id: expanded) {
            guard expanded, !isSeen else { return }
            try? await Task.sleep(for: .seconds(1.5))
            guard !Task.isCancelled else { return }
            seen = true
            await model.markSeen([message.messageId], seen: true)
        }
    }

    private var recipients: String {
        var parts: [String] = []
        if !message.to.isEmpty { parts.append("To: " + message.to.joined(separator: ", ")) }
        if !message.cc.isEmpty { parts.append("Cc: " + message.cc.joined(separator: ", ")) }
        return parts.joined(separator: " · ")
    }

    private var tools: some View {
        ViewThatFits(in: .horizontal) {
            toolRow.labelStyle(.titleAndIcon)
            toolRow.labelStyle(.iconOnly)
        }
        .buttonStyle(.glass)
        .font(.subheadline.weight(.semibold))
    }

    private var toolRow: some View {
        HStack(spacing: 8) {
            Button("Reply", systemImage: "arrowshape.turn.up.left") {
                Task { await model.compose(.reply, messageId: message.messageId) }
            }
            Button("Reply all", systemImage: "arrowshape.turn.up.left.2") {
                Task { await model.compose(.replyAll, messageId: message.messageId) }
            }
            Button("Forward", systemImage: "arrowshape.turn.up.right") {
                Task { await model.compose(.forward, messageId: message.messageId) }
            }
            Menu {
                Button("Talk about this", systemImage: "bubble.left.and.text.bubble.right") {
                    Task { await model.talkAbout(messageId: message.messageId) }
                }
                Button("Mark unread", systemImage: "envelope.badge") {
                    seen = false
                    expanded = false
                    Task { await model.markSeen([message.messageId], seen: false) }
                }
            } label: {
                Label("More", systemImage: "ellipsis")
            }
            .menuStyle(.button)
        }
        .lineLimit(1)
        .fixedSize()
    }
}

/// A message's page, its notes (not downloaded yet, images held back) and
/// its attachments.
private struct MessageBody: View {
    @Environment(AppModel.self) private var model
    let messageId: Int64
    let downloaded: Int

    @State private var rendered: RenderedMessageDto?
    @State private var allowRemote = false
    @State private var height: CGFloat = 40
    @State private var preview: URL?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let rendered {
                if !rendered.complete {
                    Label("Not downloaded yet — showing the text.", systemImage: "icloud.and.arrow.down")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if rendered.remoteResources > 0 && !allowRemote {
                    HStack {
                        Text("Remote images blocked.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Button("Load images") { allowRemote = true }
                            .font(.caption.weight(.semibold))
                    }
                }
                MailWebView(html: rendered.html, height: $height)
                    .frame(height: height)
                    .clipShape(.rect(cornerRadius: 8))
                if !rendered.attachments.isEmpty {
                    ForEach(rendered.attachments, id: \.part) { attachment in
                        Button {
                            Task { await open(attachment) }
                        } label: {
                            Label(
                                "\(attachment.name) (\(ByteCountFormatter.string(fromByteCount: Int64(attachment.size), countStyle: .file)))",
                                systemImage: "paperclip"
                            )
                            .font(.subheadline)
                            .lineLimit(1)
                        }
                        .buttonStyle(.glass)
                    }
                }
            } else {
                ProgressView().frame(maxWidth: .infinity)
            }
        }
        .task(id: "\(downloaded)-\(allowRemote)") { await render() }
        .quickLookPreview($preview)
    }

    private func render() async {
        guard let engine = model.engine else { return }
        do {
            rendered = try await engine.renderMessage(messageId: messageId, allowRemote: allowRemote)
        } catch {
            model.report(error)
        }
    }

    /// Saved to the caches, then shown in Quick Look (which also shares it).
    private func open(_ attachment: AttachmentDto) async {
        guard let engine = model.engine else { return }
        do {
            let data = try await engine.attachment(messageId: messageId, part: attachment.part)
            let dir = URL.cachesDirectory.appending(path: "attachments/\(messageId)-\(attachment.part)", directoryHint: .isDirectory)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let name = data.name.replacingOccurrences(of: "/", with: "_")
            let file = dir.appending(path: name.isEmpty ? "attachment" : name)
            try data.data.write(to: file)
            preview = file
        } catch {
            model.report(error)
        }
    }
}
