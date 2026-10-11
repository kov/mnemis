import SwiftUI
import WebKit

/// A message page from the engine, shown as part of the thread rather than
/// as a scrolling pane: it reports its height, and a page wider than the
/// screen is zoomed out to fit, as in Mail. No script of the page ever runs
/// (content JavaScript is off, and the page's own policy forbids scripts),
/// nothing is stored, and links open in the default browser instead of
/// loading here.
struct MailWebView: UIViewRepresentable {
    let html: String
    @Binding var height: CGFloat

    func makeCoordinator() -> Coordinator { Coordinator(height: $height) }

    func makeUIView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        config.websiteDataStore = .nonPersistent()
        config.dataDetectorTypes = []
        config.defaultWebpagePreferences.preferredContentMode = .mobile
        let view = FittingWebView(frame: .zero, configuration: config)
        // WebKit lays the page out for the width it had when loading and
        // scales it when the width changes, so a new width loads it again.
        view.widthChanged = { [weak coordinator = context.coordinator, weak view] in
            guard let coordinator, let view else { return }
            coordinator.load(view)
        }
        view.navigationDelegate = context.coordinator
        view.uiDelegate = context.coordinator
        // Plain text mail is transparent, to follow the app's appearance;
        // HTML mail brings its own white page.
        view.isOpaque = false
        view.backgroundColor = .clear
        view.scrollView.backgroundColor = .clear
        view.scrollView.isScrollEnabled = false
        view.scrollView.bounces = false
        context.coordinator.watch(view)
        return view
    }

    func updateUIView(_ view: WKWebView, context: Context) {
        context.coordinator.height = $height
        guard context.coordinator.html != html else { return }
        context.coordinator.html = html
        context.coordinator.load(view)
    }

    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate {
        var height: Binding<CGFloat>
        var html: String?
        /// The page itself has loaded; every later navigation is a link.
        var loaded = false
        private var observation: NSKeyValueObservation?

        init(height: Binding<CGFloat>) {
            self.height = height
        }

        /// Loads the page, once the view is wide enough to lay it out.
        func load(_ view: WKWebView) {
            guard let html, view.bounds.width >= 100 else { return }
            loaded = false
            view.pageZoom = 1
            view.loadHTMLString(html, baseURL: nil)
        }

        /// Follows the page's size: zooms a too-wide page out to fit, and
        /// reports the height (images arriving later change it too).
        func watch(_ view: WKWebView) {
            observation = view.scrollView.observe(\.contentSize, options: [.new]) { [weak self, weak view] _, _ in
                DispatchQueue.main.async {
                    guard let self, let view else { return }
                    self.fit(view)
                }
            }
        }

        /// Reports the height; the zoom is set once the page has loaded.
        func fit(_ view: WKWebView) {
            let size = view.scrollView.contentSize
            guard loaded, size.height > 0 else { return }
            let fitted = max(40, ceil(size.height))
            if abs(fitted - height.wrappedValue) > 1 {
                height.wrappedValue = fitted
            }
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            // The page's own width, asked of the page: the scroll view's size
            // passes through WebKit's 980-point default before settling. This
            // runs as the app; the page's scripts stay off.
            webView.evaluateJavaScript("document.documentElement.scrollWidth") { [weak self, weak webView] result, _ in
                guard let self, let webView else { return }
                if let pageWidth = result as? Double, pageWidth > 0, webView.bounds.width > 0 {
                    webView.pageZoom = max(0.3, min(1, webView.bounds.width / pageWidth))
                }
                self.loaded = true
                self.fit(webView)
            }
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor action: WKNavigationAction,
            decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void
        ) {
            if !loaded && action.navigationType == .other && action.targetFrame?.isMainFrame == true {
                decisionHandler(.allow)
                return
            }
            decisionHandler(.cancel)
            if action.navigationType == .linkActivated, let url = action.request.url {
                Self.openOutside(url)
            }
        }

        /// `target=_blank` links ask for a new view; open them outside too.
        func webView(
            _ webView: WKWebView,
            createWebViewWith configuration: WKWebViewConfiguration,
            for action: WKNavigationAction,
            windowFeatures: WKWindowFeatures
        ) -> WKWebView? {
            if let url = action.request.url { Self.openOutside(url) }
            return nil
        }

        /// The default browser (or mail app, for mailto:) — whatever the
        /// user set, never a hardcoded one.
        static func openOutside(_ url: URL) {
            guard let scheme = url.scheme?.lowercased(), ["http", "https", "mailto", "tel"].contains(scheme) else {
                return
            }
            UIApplication.shared.open(url)
        }
    }
}

/// Says when its width changes, so the page can be fitted again.
private final class FittingWebView: WKWebView {
    var widthChanged: (() -> Void)?
    private var lastWidth: CGFloat = 0

    override func layoutSubviews() {
        super.layoutSubviews()
        if abs(bounds.width - lastWidth) > 0.5 {
            lastWidth = bounds.width
            widthChanged?()
        }
    }
}
