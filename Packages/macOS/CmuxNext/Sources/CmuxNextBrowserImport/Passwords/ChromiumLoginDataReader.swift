public import Foundation

/// One saved sign-in read from a source browser. The password is
/// `SecretBytes`; the rest is what the source shows in its password list.
/// Descriptions are redacted so a stray print or log line carries nothing.
public struct ImportedLogin: Sendable, CustomStringConvertible, CustomReflectable {
    /// The page the form was on (`origin_url`).
    public var url: String
    /// Chromium's match key (`signon_realm`), for example "https://github.com/".
    public var signonRealm: String
    public var username: String
    public var password: SecretBytes
    public var created: Date?

    public init(url: String, signonRealm: String, username: String, password: SecretBytes, created: Date?) {
        self.url = url
        self.signonRealm = signonRealm
        self.username = username
        self.password = password
        self.created = created
    }

    public var description: String { "ImportedLogin(<redacted>)" }
    public var customMirror: Mirror { Mirror(self, children: []) }
}

/// What a read left out, by reason. Counts only.
public struct LoginSkipCounts: Sendable, Equatable, Codable {
    /// "Never save" entries: a site, no password.
    public var neverSaved = 0
    /// Sign in with another provider (federated), Android app and HTTP auth entries.
    public var notWebForm = 0
    /// No password (username-only entries).
    public var empty = 0
    /// The key did not open the value.
    public var undecryptable = 0
    /// The same site and username twice in this source (the newest is kept).
    public var duplicate = 0

    public init() {}

    public var total: Int { neverSaved + notWebForm + empty + undecryptable + duplicate }
}

// CMUX_NO_PASSWORD_IMPORT (set only by the cx-f58x notary test build,
// nightly.yml input notary_test_without_password_import) compiles out the
// browser password readers. Default builds include them.
#if !CMUX_NO_PASSWORD_IMPORT
/// A Chromium profile's saved passwords: `Login Data` (the profile store) and
/// `Login Data For Account` (the account store). The sandboxed import service
/// reads each database's rows with the passwords still sealed
/// (CmuxNextBrowserImportParsers); here each is opened in memory into
/// `SecretBytes` with the browser's Safe Storage key, which never leaves the
/// app. Nothing here writes a password anywhere.
public struct ChromiumLoginDataReader {
    public init() {}

    public static let files = ["Login Data", "Login Data For Account"]

    /// Whether the profile has a login database at all (detection reads no rows).
    public static func hasLogins(_ profile: URL) -> Bool {
        files.contains { FileManager.default.fileExists(atPath: profile.appending(path: $0).path) }
    }

    /// The profile's login databases that exist, in ``files`` order.
    public func databases(profile: URL) -> [URL] {
        Self.files.map { profile.appending(path: $0) }.filter { FileManager.default.fileExists(atPath: $0.path) }
    }

    /// Opens each database's sealed sign-ins, in order: a value the key does not open, or that opens
    /// empty, is counted; the same site and username twice keeps the newest.
    public func open(_ databases: [SealedLogins], crypto: ChromiumPasswordCrypto) -> (logins: [ImportedLogin], skipped: LoginSkipCounts) {
        var skipped = LoginSkipCounts()
        var byKey: [String: ImportedLogin] = [:]
        var order: [String] = []
        for database in databases {
            skipped.neverSaved += database.skipped.neverSaved
            skipped.notWebForm += database.skipped.notWebForm
            skipped.empty += database.skipped.empty
            for sealed in database.logins {
                let password: SecretBytes
                do {
                    password = try crypto.decrypt(sealed.sealedPassword)
                } catch {
                    skipped.undecryptable += 1
                    continue
                }
                guard !password.isEmpty else {
                    skipped.empty += 1
                    continue
                }
                let login = ImportedLogin(url: sealed.url, signonRealm: sealed.signonRealm, username: sealed.username, password: password,
                                          created: sealed.created)
                let key = sealed.signonRealm + "\u{0}" + login.username
                if let kept = byKey[key] {
                    skipped.duplicate += 1
                    if (login.created ?? .distantPast) > (kept.created ?? .distantPast) { byKey[key] = login }
                } else {
                    byKey[key] = login
                    order.append(key)
                }
            }
        }
        return (order.compactMap { byKey[$0] }, skipped)
    }
}
#endif
