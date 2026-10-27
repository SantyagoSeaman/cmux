public import Foundation

/// One saved sign-in of a Chromium `Login Data` database, its password still sealed with the
/// browser's Safe Storage key.
public struct SealedLogin: Sendable, Codable, Equatable {
    public var url: String
    public var signonRealm: String
    public var username: String
    public var sealedPassword: Data
    public var created: Date?

    public init(url: String, signonRealm: String, username: String, sealedPassword: Data, created: Date?) {
        self.url = url
        self.signonRealm = signonRealm
        self.username = username
        self.sealedPassword = sealedPassword
        self.created = created
    }
}

/// One `Login Data` database's web-form sign-ins, sealed, and the rows that are not one (never
/// saved, not a web form, no password).
public struct SealedLogins: Sendable, Codable, Equatable {
    public var logins: [SealedLogin]
    public var skipped: LoginSkipCounts

    public init(logins: [SealedLogin] = [], skipped: LoginSkipCounts = LoginSkipCounts()) {
        self.logins = logins
        self.skipped = skipped
    }
}

/// One `logins.json` entry, its username and password still sealed (base64 NSS DER).
public struct FirefoxLoginEntry: Sendable, Codable, Equatable {
    public var hostname: String
    public var httpRealm: String?
    public var encryptedUsername: String
    public var encryptedPassword: String
    /// Milliseconds since 1970.
    public var timeCreated: Double?

    public init(hostname: String, httpRealm: String?, encryptedUsername: String, encryptedPassword: String, timeCreated: Double?) {
        self.hostname = hostname
        self.httpRealm = httpRealm
        self.encryptedUsername = encryptedUsername
        self.encryptedPassword = encryptedPassword
        self.timeCreated = timeCreated
    }
}
