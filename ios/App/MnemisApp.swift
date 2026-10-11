import SwiftUI

@main
struct MnemisApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    init() {
        #if DEBUG
        // Line by line, so `devicectl … --console` shows prints as they
        // happen rather than when a buffer fills (or never, if killed).
        setvbuf(stdout, nil, _IOLBF, 0)
        #endif
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(model)
                .task { await model.start() }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                AppModel.scheduleBackgroundSync()
            }
        }
        .backgroundTask(.appRefresh(AppModel.backgroundSyncTask)) {
            await model.backgroundSync()
        }
    }
}
