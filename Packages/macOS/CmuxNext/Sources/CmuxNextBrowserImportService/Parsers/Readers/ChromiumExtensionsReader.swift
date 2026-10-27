public import CmuxNextBrowserImport
public import Foundation

/// Reads the files a Chromium profile's extensions are listed from: the `extensions.settings.<id>`
/// entries of `Preferences` and `Secure Preferences` (``ChromiumSearchEngineReader/preferences(_:)``
/// reads the file), each version folder's `manifest.json`, and a locale's `messages.json`. Runs in
/// the sandboxed import service; the App lists the folders and merges (`ProfileExtensions`).
public struct ChromiumExtensionsReader {
    public init() {}

    /// The fields of one `extensions.settings.<id>` entry the import reads.
    static func settings(_ entry: [String: Any]) -> ChromiumExtensionSettings {
        ChromiumExtensionSettings(location: entry["location"] as? Int, fromWebStore: entry["from_webstore"] as? Bool,
                                  wasInstalledByDefault: entry["was_installed_by_default"] as? Bool,
                                  wasInstalledByOEM: entry["was_installed_by_oem"] as? Bool, state: entry["state"] as? Int,
                                  disableReasons: entry["disable_reasons"] as? Int,
                                  manifest: (entry["manifest"] as? [String: Any]).map(manifest))
    }

    /// A `manifest.json`; nil when it is not a JSON object.
    public func manifest(_ data: Data) -> ChromiumExtensionManifest? {
        guard let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return Self.manifest(json)
    }

    static func manifest(_ json: [String: Any]) -> ChromiumExtensionManifest {
        ChromiumExtensionManifest(name: json["name"] as? String, hasName: json["name"] != nil, version: json["version"] as? String,
                                  defaultLocale: json["default_locale"] as? String, updateURL: json["update_url"] as? String,
                                  isTheme: json["theme"] != nil)
    }

    /// The message whose name is `key` in a `messages.json` (names are case-insensitive; `key` is lowercased).
    public func message(_ data: Data, key: String) -> String? {
        guard let messages = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let entry = messages.first(where: { $0.key.lowercased() == key })?.value as? [String: Any],
              let message = entry["message"] as? String, !message.isEmpty else { return nil }
        return message
    }
}
