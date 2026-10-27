import CmuxNextCompat
public import Foundation

/// Makes the connection a parse goes over: the bundled XPC service in the app, or an anonymous
/// listener in tests.
public protocol BrowserImportServiceConnecting: Sendable {
    /// False when there is no service to connect to (a build that does not bundle it).
    func isAvailable() -> Bool
    /// A new connection, not yet resumed.
    func makeConnection() -> NSXPCConnection
}

/// The service bundled in the app at `Contents/XPCServices/cmux-browser-import.xpc`: the Xcode
/// target `cmux-browser-import`, embedded and signed with its sandbox entitlements by the app's
/// phase "Embed browser import service" (scripts/cmux-next/sign-browser-import-service.sh).
public struct BundledBrowserImportService: BrowserImportServiceConnecting {
    /// The service bundle's identifier (its Info.plist). An app's XPC services are looked up in its
    /// own bundle, so every build variant (DEV tags, NIGHTLY, Release) uses the same name.
    public static let serviceName = "com.cmuxterm.browser-import"
    /// The bundle's folder name in `Contents/XPCServices`.
    public static let bundleName = "cmux-browser-import.xpc"

    private let app: URL

    public init(app: URL = Bundle.main.bundleURL) {
        self.app = app
    }

    public func isAvailable() -> Bool {
        FileManager.checked.fileExists(atPath: app.appending(path: "Contents/XPCServices/\(Self.bundleName)").path)
    }

    public func makeConnection() -> NSXPCConnection {
        NSXPCConnection(serviceName: Self.serviceName)
    }
}
