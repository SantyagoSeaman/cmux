public import CmuxNextBrowserImport
import CmuxNextSQLiteSnapshot
public import Foundation

// CMUX_NO_PASSWORD_IMPORT (set only by the cx-f58x notary test build) compiles out the browser
// password readers, here as in CmuxNextBrowserImport.
#if !CMUX_NO_PASSWORD_IMPORT
/// Reads one Chromium `Login Data` database through a private copy: each web-form sign-in with
/// its password still sealed. Runs in the sandboxed import service, which has no key; the App
/// opens the passwords (``ChromiumLoginDataReader/open(_:crypto:)``).
public struct ChromiumLoginRows {
    public init() {}

    /// The database's sign-ins; a database with no `logins` table has none.
    public func read(_ file: URL) throws -> SealedLogins {
        let database = try SQLiteSnapshot(copying: file)
        var read = SealedLogins()
        guard database.hasTable("logins") else { return read }
        let sql = "SELECT origin_url, signon_realm, username_value, password_value, date_created, blacklisted_by_user, scheme, "
            + "federation_url FROM logins"
        try database.query(sql) { row in
            let realm = row.string(1) ?? ""
            // scheme 0 is an HTML form; 1-3 are HTTP auth. Federated and Android entries have no web password.
            guard row.int64(5) == 0 else {
                read.skipped.neverSaved += 1
                return true
            }
            guard row.int64(6) == 0, (row.string(7) ?? "").isEmpty, realm.hasPrefix("http") else {
                read.skipped.notWebForm += 1
                return true
            }
            guard let sealed = row.data(3), !sealed.isEmpty else {
                read.skipped.empty += 1
                return true
            }
            read.logins.append(SealedLogin(url: row.string(0) ?? realm, signonRealm: realm, username: row.string(2) ?? "",
                                           sealedPassword: sealed, created: BrowserTime().chromium(row.int64(4))))
            return true
        }
        return read
    }
}

/// Reads Firefox's NSS key store (`key4.db`) through a private copy: the global salt, the
/// password-check value and each master key, all still sealed with the primary password.
public struct FirefoxKeyStoreReader {
    public init() {}

    public func read(_ file: URL) throws -> FirefoxKeyStore {
        let key4 = try SQLiteSnapshot(copying: file)
        guard key4.hasTable("metaData"), key4.hasTable("nssPrivate") else { throw FirefoxPasswordCrypto.Failure.malformed }
        var salt: Data?
        var check: Data?
        try key4.query("SELECT item1, item2 FROM metaData WHERE id = 'password'") { row in
            salt = row.data(0)
            check = row.data(1)
            return false
        }
        guard let salt, let check else { throw FirefoxPasswordCrypto.Failure.malformed }
        var keys: [FirefoxKeyStore.SealedKey] = []
        try key4.query("SELECT a11, a102 FROM nssPrivate") { row in
            if let sealed = row.data(0), let id = row.data(1) { keys.append(FirefoxKeyStore.SealedKey(sealed: sealed, id: id)) }
            return true
        }
        return FirefoxKeyStore(globalSalt: salt, passwordCheck: check, keys: keys)
    }
}

/// Reads Firefox's `logins.json`: the entries with their usernames and passwords still sealed.
public struct FirefoxLoginsFile {
    public init() {}

    private struct File: Decodable {
        struct Entry: Decodable {
            var hostname: String
            var httpRealm: String?
            var encryptedUsername: String
            var encryptedPassword: String
            /// Milliseconds since 1970.
            var timeCreated: Double?
        }

        var logins: [Entry]
    }

    public func parse(_ data: Data) throws -> [FirefoxLoginEntry] {
        try JSONDecoder().decode(File.self, from: data).logins.map { entry in
            FirefoxLoginEntry(hostname: entry.hostname, httpRealm: entry.httpRealm, encryptedUsername: entry.encryptedUsername,
                              encryptedPassword: entry.encryptedPassword, timeCreated: entry.timeCreated)
        }
    }
}
#endif
