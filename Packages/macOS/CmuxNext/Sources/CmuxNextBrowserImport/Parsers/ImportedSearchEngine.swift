public import Foundation

/// A source profile's default search engine: Chromium's name, keyword and
/// address templates (`{searchTerms}` marks the typed text).
public struct ImportedSearchEngine: Sendable, Codable, Equatable {
    public var name: String
    public var keyword: String?
    public var search: String
    public var suggest: String?

    public init(name: String, keyword: String? = nil, search: String, suggest: String? = nil) {
        self.name = name
        self.keyword = keyword
        self.search = search
        self.suggest = suggest
    }

    /// What `browser.searchEngine` becomes: the built-in engine whose search
    /// address has this engine's host (`builtIn`: engine id to its search
    /// template), else `custom` with this engine's addresses.
    public func setting(builtIn: [String: String]) -> (engine: String, search: String?, suggest: String?) {
        let host = Self.host(search)
        if let host, let match = builtIn.first(where: { Self.host($0.value) == host }) { return (match.key, nil, nil) }
        return ("custom", search, suggest)
    }

    package static func host(_ template: String) -> String? {
        let url = URL(string: template.replacingOccurrences(of: "{searchTerms}", with: "x").replacingOccurrences(of: "%s", with: "x"))
        guard let host = url?.host()?.lowercased() else { return nil }
        return host.hasPrefix("www.") ? String(host.dropFirst(4)) : host
    }

    /// Whether `template` is an address the import keeps: https, and it takes the typed text.
    package static func takesTypedText(_ template: String) -> Bool {
        template.contains("{searchTerms}") && template.lowercased().hasPrefix("https://")
    }
}
