public import Foundation

/// Lists the Chrome Web Store extensions of a Chromium profile.
///
/// Sources, merged per id: `Secure Preferences` and `Preferences`
/// (`extensions.settings.<id>`: location, from_webstore, state,
/// was_installed_by_default) and the unpacked copies under
/// `Extensions/<id>/<version>/manifest.json` (name, resolving `__MSG_x__`
/// through `_locales/<default_locale>/messages.json`). Component, policy,
/// default-installed and unpacked extensions are left out: they cannot be
/// reinstalled from the store. The App lists the folders and merges; the
/// sandboxed import service parses each JSON file (`reader`).
public struct ProfileExtensions: Sendable {
    private let reader: any ProfileDataReading

    public init(reader: any ProfileDataReading) {
        self.reader = reader
    }

    /// Chrome's own apps that ship with every profile.
    static let builtIns: Set<String> = [
        "nmmhkkegccagdldgiimedpiccmgmieda",  // Chrome Web Store Payments
        "pjkljhegncpnkpknbcohdijeoejaedia",  // Gmail (legacy app)
        "blpcfgokakmgnkcojhhkbfbldkacnbeo",  // YouTube (legacy app)
        "apdfllckaahabafndbhieahigkjlhalf",  // Google Drive (legacy app)
        "aapocclcgogkmnckokdopfmhonfmgoek",  // Slides
        "aohghmighlieiainnegkcijnfilokake",  // Docs
        "felcaaldnbdncclmgdcncolpebgiejap",  // Sheets
        "ahfgeienlihckogmohjhadlkjgocpleb",  // Web Store app
    ]

    /// Chromium `Manifest::Location` values that can come from the store:
    /// INTERNAL (1), EXTERNAL_PREF_DOWNLOAD (6), EXTERNAL_POLICY_DOWNLOAD is
    /// policy (7, excluded).
    static let storeLocations: Set<Int> = [1, 6]

    public func read(profile: URL) async -> [ImportedExtension] {
        var settings: [String: ChromiumExtensionSettings] = [:]
        for name in ["Preferences", "Secure Preferences"] {
            guard let preferences = try? await reader.chromiumPreferences(profile.appending(path: name)) else { continue }
            for (id, entry) in preferences.extensions {
                settings[id] = settings[id].map { $0.merged(with: entry) } ?? entry
            }
        }
        let installed = profile.appending(path: "Extensions")
        let folders = (try? FileManager.default.contentsOfDirectory(atPath: installed.path)) ?? []
        let ids = Set(settings.keys).union(folders).filter(ImportedExtension.isValidID).subtracting(Self.builtIns)
        var found: [ImportedExtension] = []
        for id in ids {
            let entry = settings[id] ?? ChromiumExtensionSettings()
            if let location = entry.location, !Self.storeLocations.contains(location) { continue }
            if entry.wasInstalledByDefault == true || entry.wasInstalledByOEM == true { continue }
            let manifest = await latestManifest(installed.appending(path: id))
            guard let name = await displayName(manifest, entry.manifest) else { continue }
            if manifest?.manifest.isTheme == true || entry.manifest?.isTheme == true { continue }  // Themes are not extensions.
            let fromStore = entry.fromWebStore ?? manifest?.manifest.updateURL?.contains("google.com") ?? true
            guard fromStore else { continue }
            let disabled = entry.state == 0 || (entry.disableReasons ?? 0) != 0
            found.append(ImportedExtension(id: id, name: name, version: manifest?.manifest.version ?? entry.manifest?.version,
                                           enabled: !disabled, fromWebStore: fromStore))
        }
        return found.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    private struct FolderManifest {
        var manifest: ChromiumExtensionManifest
        var folder: URL
    }

    /// The manifest in the highest version folder that has one.
    private func latestManifest(_ extensionFolder: URL) async -> FolderManifest? {
        let versions = (try? FileManager.default.contentsOfDirectory(atPath: extensionFolder.path)) ?? []
        for version in versions.sorted(by: { $0.compare($1, options: .numeric) == .orderedDescending }) {
            let folder = extensionFolder.appending(path: version, directoryHint: .isDirectory)
            if let manifest = try? await reader.extensionManifest(folder.appending(path: "manifest.json")) {
                return FolderManifest(manifest: manifest, folder: folder)
            }
        }
        return nil
    }

    /// The manifest's name (the settings' copy when the folder's manifest has none), with a
    /// `__MSG_x__` name looked up in the manifest's locale, then English.
    private func displayName(_ manifest: FolderManifest?, _ settingsManifest: ChromiumExtensionManifest?) async -> String? {
        let source = manifest.map(\.manifest).flatMap { $0.hasName ? $0 : nil } ?? settingsManifest
        guard let raw = source?.name, !raw.isEmpty else { return nil }
        guard raw.hasPrefix("__MSG_"), raw.hasSuffix("__"), let manifest else { return raw }
        let key = String(raw.dropFirst(6).dropLast(2)).lowercased()
        for locale in [manifest.manifest.defaultLocale, "en", "en_US"].compactMap(\.self) {
            let file = manifest.folder.appending(path: "_locales/\(locale)/messages.json")
            if let message = try? await reader.extensionMessage(file, key: key) { return message }
        }
        return nil
    }
}
