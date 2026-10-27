public import Foundation

// CMUX_NO_PASSWORD_IMPORT (set only by the cx-f58x notary test build,
// nightly.yml input notary_test_without_password_import) compiles out the
// browser password readers. Default builds include them.
#if !CMUX_NO_PASSWORD_IMPORT
/// A Firefox-family profile's saved passwords: `logins.json` (the encrypted
/// entries) and `key4.db` (the NSS key store). The sandboxed import service
/// reads both files with every value still sealed
/// (CmuxNextBrowserImportParsers); here the key store is opened with the
/// primary password, which never leaves the app, and each password is
/// decrypted in memory into `SecretBytes`, the username into an ordinary
/// string. Nothing here writes a password anywhere.
public struct FirefoxLoginReader {
    public init() {}

    /// Whether the profile has a login store at all (detection reads no rows).
    public static func hasLogins(_ profile: URL) -> Bool {
        ["logins.json", "key4.db"].allSatisfy { FileManager.default.fileExists(atPath: profile.appending(path: $0).path) }
    }

    /// Opens `entries` with the key store `crypto` opened (``FirefoxPasswordCrypto/init(keyStore:primaryPassword:)``
    /// asks for the primary password); throws only when the task is cancelled.
    public func open(_ entries: [FirefoxLoginEntry], crypto: FirefoxPasswordCrypto) throws -> (logins: [ImportedLogin], skipped: LoginSkipCounts) {
        var skipped = LoginSkipCounts()
        var byKey: [String: ImportedLogin] = [:]
        var order: [String] = []
        for (index, entry) in entries.enumerated() {
            if index % 256 == 255 { try Task.checkCancellation() }
            // HTTP authentication rows carry a realm; extension and about: origins are not web forms.
            guard entry.httpRealm == nil, let (url, realm) = PasswordCSVReader.webForm(entry.hostname) else {
                skipped.notWebForm += 1
                continue
            }
            let password: SecretBytes
            let username: String
            do {
                password = try crypto.decrypt(entry.encryptedPassword)
                username = try crypto.decrypt(entry.encryptedUsername).withUnsafeBytes { bytes in
                    // SAFETY: String copies the view's count bytes while the closure runs and keeps no pointer.
                    unsafe String(decoding: bytes, as: UTF8.self)
                }
            } catch {
                skipped.undecryptable += 1
                continue
            }
            guard password.isValidUTF8 else {
                skipped.undecryptable += 1
                continue
            }
            guard !password.isEmpty else {
                skipped.empty += 1
                continue
            }
            let created = entry.timeCreated.flatMap { BrowserTime().unix($0 / 1000) }
            let login = ImportedLogin(url: url, signonRealm: realm, username: username, password: password, created: created)
            let key = realm + "\u{0}" + username
            if let kept = byKey[key] {
                skipped.duplicate += 1
                if (created ?? .distantPast) > (kept.created ?? .distantPast) { byKey[key] = login }
            } else {
                byKey[key] = login
                order.append(key)
            }
        }
        return (order.compactMap { byKey[$0] }, skipped)
    }
}
#endif
