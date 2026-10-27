public import Foundation

// The values the sandboxed import service reads out of another browser's files, before the app
// opens or merges them (plans/cmux-next/rule-of-2.md). Secrets stay in the app: Chromium's cookie
// and password values cross sealed, and are opened here with the Safe Storage key the app reads
// from the Keychain; Firefox's key store crosses sealed, and the primary password never leaves the
// app. Chrome splits its importer the same way, parse in a utility process and merge in the
// browser process (chrome/utility/importer/profile_import_impl.cc, in_process_importer_bridge.cc);
// cmux also keeps every key in the app.

/// One cookie as the source stores it. `sealedValue` is Chromium's `encrypted_value` when the row
/// has one (`cookie.value` is then empty); every other cookie is plain.
public struct SealedCookie: Sendable, Codable, Equatable {
    public var cookie: ImportedCookie
    public var sealedValue: Data?

    public init(cookie: ImportedCookie, sealedValue: Data? = nil) {
        self.cookie = cookie
        self.sealedValue = sealedValue
    }
}

/// A cookie store's rows: the cookies, sealed or plain, and the partitioned (CHIPS) or container
/// cookies left out.
public struct SealedCookies: Sendable, Codable, Equatable {
    public var cookies: [SealedCookie]
    /// Chromium's `meta.version`: from 24 a value starts with SHA-256 of its host. 0 for other stores.
    public var databaseVersion: Int
    public var partitioned: Int

    public init(cookies: [SealedCookie] = [], databaseVersion: Int = 0, partitioned: Int = 0) {
        self.cookies = cookies
        self.databaseVersion = databaseVersion
        self.partitioned = partitioned
    }

    /// The cookies with their values opened with `crypto` (nil: no key, so only plain cookies read),
    /// and the ones that are expired at `now` counted. A store whose sealed values none open is the
    /// wrong key or a scheme cmux does not know.
    public func opened(crypto: ChromiumCookieCrypto?, now: Date) throws -> CookieReadResult {
        var result = CookieReadResult(cookies: [], partitioned: partitioned)
        for row in cookies {
            var cookie = row.cookie
            if let sealed = row.sealedValue {
                guard let crypto, let plain = try? crypto.decrypt(sealed, hostKey: cookie.domain, databaseVersion: databaseVersion) else {
                    result.undecryptable += 1
                    continue
                }
                cookie.value = plain
            }
            if cookie.isExpired(at: now) { result.expired += 1 } else { result.cookies.append(cookie) }
        }
        if result.cookies.isEmpty, result.undecryptable > 0 { throw CookieImportError.undecryptable }
        return result
    }
}
