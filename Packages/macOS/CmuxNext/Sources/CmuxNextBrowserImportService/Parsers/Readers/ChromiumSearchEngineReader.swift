public import CmuxNextBrowserImport
import CmuxNextSQLiteSnapshot
public import Foundation

/// Reads a Chromium profile's `Preferences` (the default search engine the person set, else the
/// GUID of its `Web Data` row, and the extension settings) and its `Web Data` `keywords` row. Only
/// https engines whose search address takes the typed text are returned (Chromium-only
/// `{google:...}` parameters are dropped). Runs in the sandboxed import service; the App picks
/// which file to read (`ProfileSearchEngine`, `ProfileExtensions`).
public struct ChromiumSearchEngineReader {
    public init() {}

    public enum Failure: Error { case notPreferences }

    /// A `Preferences` or `Secure Preferences` file.
    public func preferences(_ data: Data) throws -> ChromiumPreferences {
        guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw Failure.notPreferences }
        var read = ChromiumPreferences()
        if let template = (root["default_search_provider_data"] as? [String: Any])?["template_url_data"] as? [String: Any] {
            read.searchEngine = Self.engine(name: template["short_name"] as? String, keyword: template["keyword"] as? String,
                                            search: template["url"] as? String, suggest: template["suggestions_url"] as? String)
        }
        read.defaultSearchGUID = ((root["default_search_provider"] as? [String: Any])?["guid"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        let settings = (root["extensions"] as? [String: Any])?["settings"] as? [String: Any] ?? [:]
        for (id, value) in settings {
            guard let entry = value as? [String: Any] else { continue }
            read.extensions[id] = ChromiumExtensionsReader.settings(entry)
        }
        return read
    }

    /// The `keywords` row of a `Web Data` database whose `sync_guid` is `guid`.
    public func engine(webData file: URL, guid: String) throws -> ImportedSearchEngine? {
        let database = try SQLiteSnapshot(copying: file)
        var found: ImportedSearchEngine?
        let quoted = guid.replacingOccurrences(of: "'", with: "''")
        try database.query("SELECT short_name, keyword, url, suggest_url FROM keywords WHERE sync_guid = '\(quoted)' LIMIT 1") { row in
            found = Self.engine(name: row.string(0), keyword: row.string(1), search: row.string(2), suggest: row.string(3))
            return false
        }
        return found
    }

    static func engine(name: String?, keyword: String?, search: String?, suggest: String?) -> ImportedSearchEngine? {
        guard let search = search.map(clean), ImportedSearchEngine.takesTypedText(search) else { return nil }
        let suggest = suggest.map(clean).flatMap { ImportedSearchEngine.takesTypedText($0) ? $0 : nil }
        let title = name.flatMap { $0.isEmpty ? nil : $0 } ?? ImportedSearchEngine.host(search) ?? search
        return ImportedSearchEngine(name: title, keyword: keyword.flatMap { $0.isEmpty ? nil : $0 }, search: search, suggest: suggest)
    }

    /// Drops Chromium's own `{google:...}` parameters and other placeholders
    /// (`{inputEncoding}`), which no other browser fills in, and any query
    /// item they leave empty (`&t=`).
    static func clean(_ template: String) -> String {
        let text = template.replacingOccurrences(of: #"\{google:[A-Za-z]+\}"#, with: "", options: .regularExpression)
        guard var parts = URLComponents(string: text.replacingOccurrences(of: "{searchTerms}", with: "CMUXSEARCHTERMS")),
              let items = parts.queryItems else { return text }
        let kept = items.filter { item in
            guard let value = item.value else { return !item.name.isEmpty }
            return !value.isEmpty && !value.contains("{")
        }
        parts.queryItems = kept.isEmpty ? nil : kept
        return (parts.string ?? text).replacingOccurrences(of: "CMUXSEARCHTERMS", with: "{searchTerms}")
    }
}
