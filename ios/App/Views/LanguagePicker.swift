import SwiftUI

/// The language mnemis writes to-dos, summaries and the briefing in. The
/// engine keeps its English name ("Portuguese (Brazil)"), which the model
/// reads best; the picker shows each in its own language. `nil` is each
/// email's own language.
struct LanguagePicker: View {
    @Binding var language: String?

    var body: some View {
        Picker("Language", selection: $language) {
            Text("Each email's own").tag(String?.none)
            ForEach(Self.options(including: language), id: \.self) { english in
                Text(Self.nativeName(english)).tag(Optional(english))
            }
        }
    }

    /// The device's languages first, then common ones, then a saved one
    /// that is neither.
    static func options(including saved: String?) -> [String] {
        let common = ["en", "pt-BR", "pt-PT", "es", "fr", "de", "it", "nl", "ja", "zh-Hans", "ko"]
        var out: [String] = []
        for id in Locale.preferredLanguages + common {
            if let english = englishName(id), !out.contains(english) {
                out.append(english)
            }
        }
        if let saved, !out.contains(saved) {
            out.append(saved)
        }
        return out
    }

    /// The device's first language, the default when onboarding.
    static var deviceLanguage: String? {
        Locale.preferredLanguages.first.flatMap(englishName)
    }

    static func englishName(_ id: String) -> String? {
        Locale(identifier: "en").localizedString(forIdentifier: id)
    }

    /// `english` in its own language, when it is one the device knows.
    static func nativeName(_ english: String) -> String {
        let ids = Locale.preferredLanguages + Locale.availableIdentifiers
        guard let id = ids.first(where: { englishName($0) == english }),
              let native = Locale(identifier: id).localizedString(forIdentifier: id)
        else { return english }
        return native.prefix(1).uppercased() + native.dropFirst()
    }
}
