public import Foundation

/// Firefox's NSS key store (`key4.db`): the global salt and the password-check value
/// (`metaData` row `password`), and each sealed master key (`nssPrivate.a11`) with its id (`a102`).
/// The app opens them with the primary password (``FirefoxPasswordCrypto``).
public struct FirefoxKeyStore: Sendable, Codable, Equatable {
    public struct SealedKey: Sendable, Codable, Equatable {
        public var sealed: Data
        public var id: Data

        public init(sealed: Data, id: Data) {
            self.sealed = sealed
            self.id = id
        }
    }

    public var globalSalt: Data
    public var passwordCheck: Data
    public var keys: [SealedKey]

    public init(globalSalt: Data, passwordCheck: Data, keys: [SealedKey]) {
        self.globalSalt = globalSalt
        self.passwordCheck = passwordCheck
        self.keys = keys
    }
}
