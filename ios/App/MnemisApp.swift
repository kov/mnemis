import SwiftUI

@main
struct MnemisApp: App {
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

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
