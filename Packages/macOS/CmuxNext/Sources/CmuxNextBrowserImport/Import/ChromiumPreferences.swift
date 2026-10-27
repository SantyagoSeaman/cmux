/// What a Chromium `Preferences` (or `Secure Preferences`) file says about the import: the default
/// search engine the person set, else the `Web Data` row that holds it, and the extension settings.
public struct ChromiumPreferences: Sendable, Codable, Equatable {
    /// `default_search_provider_data.template_url_data`, when it is an https engine that takes the typed text.
    public var searchEngine: ImportedSearchEngine?
    /// `default_search_provider.guid`: the `keywords` row of `Web Data` with the default engine.
    public var defaultSearchGUID: String?
    /// `extensions.settings`, by extension id.
    public var extensions: [String: ChromiumExtensionSettings]

    public init(searchEngine: ImportedSearchEngine? = nil, defaultSearchGUID: String? = nil,
                extensions: [String: ChromiumExtensionSettings] = [:]) {
        self.searchEngine = searchEngine
        self.defaultSearchGUID = defaultSearchGUID
        self.extensions = extensions
    }
}

/// One `extensions.settings.<id>` entry: the fields the import reads (nil: the file has none).
public struct ChromiumExtensionSettings: Sendable, Codable, Equatable {
    /// `Manifest::Location`.
    public var location: Int?
    public var fromWebStore: Bool?
    public var wasInstalledByDefault: Bool?
    public var wasInstalledByOEM: Bool?
    public var state: Int?
    public var disableReasons: Int?
    /// The `manifest` copy some entries carry.
    public var manifest: ChromiumExtensionManifest?

    public init(location: Int? = nil, fromWebStore: Bool? = nil, wasInstalledByDefault: Bool? = nil, wasInstalledByOEM: Bool? = nil,
                state: Int? = nil, disableReasons: Int? = nil, manifest: ChromiumExtensionManifest? = nil) {
        self.location = location
        self.fromWebStore = fromWebStore
        self.wasInstalledByDefault = wasInstalledByDefault
        self.wasInstalledByOEM = wasInstalledByOEM
        self.state = state
        self.disableReasons = disableReasons
        self.manifest = manifest
    }

    /// `newer`'s fields over these, as `Secure Preferences` overrides `Preferences` key by key.
    public func merged(with newer: ChromiumExtensionSettings) -> ChromiumExtensionSettings {
        ChromiumExtensionSettings(location: newer.location ?? location, fromWebStore: newer.fromWebStore ?? fromWebStore,
                                  wasInstalledByDefault: newer.wasInstalledByDefault ?? wasInstalledByDefault,
                                  wasInstalledByOEM: newer.wasInstalledByOEM ?? wasInstalledByOEM, state: newer.state ?? state,
                                  disableReasons: newer.disableReasons ?? disableReasons, manifest: newer.manifest ?? manifest)
    }
}

/// The fields of an extension's `manifest.json` the import reads.
public struct ChromiumExtensionManifest: Sendable, Codable, Equatable {
    /// `name` when it is a string; `hasName` is whether the field is there at all (a name that is
    /// not a string still hides the settings' copy, as the manifest is read first).
    public var name: String?
    public var hasName: Bool
    public var version: String?
    public var defaultLocale: String?
    public var updateURL: String?
    public var isTheme: Bool

    public init(name: String?, hasName: Bool, version: String?, defaultLocale: String?, updateURL: String?, isTheme: Bool) {
        self.name = name
        self.hasName = hasName
        self.version = version
        self.defaultLocale = defaultLocale
        self.updateURL = updateURL
        self.isTheme = isTheme
    }
}
