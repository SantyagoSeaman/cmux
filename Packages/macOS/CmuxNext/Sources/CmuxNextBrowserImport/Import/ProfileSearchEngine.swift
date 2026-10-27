public import Foundation

/// A source profile's default search engine. The App picks the files from the profile folder; the
/// sandboxed import service parses them (`reader`).
public struct ProfileSearchEngine: Sendable {
    private let reader: any ProfileDataReading

    public init(reader: any ProfileDataReading) {
        self.reader = reader
    }

    /// Chromium: the engine `Preferences` names, else the `Web Data` row whose sync GUID it names.
    /// Nil when the profile keeps the browser's own default, or a file does not read.
    public func chromium(profile: URL) async -> ImportedSearchEngine? {
        guard let preferences = try? await reader.chromiumPreferences(profile.appending(path: "Preferences")) else { return nil }
        if let engine = preferences.searchEngine { return engine }
        guard let guid = preferences.defaultSearchGUID else { return nil }
        return try? await reader.searchEngine(.chromiumSearchEngine, file: profile.appending(path: "Web Data"), key: guid)
    }

    /// Firefox: the engine `search.json.mozlz4` names. Nil when it names none, or does not read.
    public func firefox(profile: URL) async -> ImportedSearchEngine? {
        try? await reader.searchEngine(.firefoxSearchEngine, file: profile.appending(path: "search.json.mozlz4"))
    }
}
