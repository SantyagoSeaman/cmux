import Foundation

// Wire shapes of cmux-tui `app-children-v1` (cmux-tui/spec/commands.md, "App children";
// cmux-tui-core/src/server/app_children.rs, p01-process-239).

/// The `app-child-*` requests; ``AppChildClient`` sends them.
enum AppChildRequests {
    struct Start: DaemonRequest {
        typealias Response = AppChildStarted
        static let commandName: DaemonCommandName = "app-child-start"
        static let requiredCapability: String? = DaemonCapability.appChildren.rawValue
        var name: String
        var owner: String
        var executable: String
        var args: [String]
        var env: [String: String]
        var cwd: String?
        var stdin: String?
        var secret: Bool
        var ready: String?
        var readyTimeoutMS: UInt64?

        enum CodingKeys: String, CodingKey {
            case name, owner, executable, args, env, cwd, stdin, secret, ready
            case readyTimeoutMS = "ready_timeout_ms"
        }
    }

    struct Read: DaemonRequest {
        typealias Response = AppChildChunk
        static let commandName: DaemonCommandName = "app-child-read"
        static let requiredCapability: String? = DaemonCapability.appChildren.rawValue
        var child: String
        var after: UInt64
        var waitMS: UInt64
        var untilExit: Bool

        enum CodingKeys: String, CodingKey {
            case child, after
            case waitMS = "wait_ms"
            case untilExit = "until_exit"
        }
    }

    struct Stop: DaemonRequest {
        typealias Response = Stopped
        static let commandName: DaemonCommandName = "app-child-stop"
        static let requiredCapability: String? = DaemonCapability.appChildren.rawValue
        var child: String
        var mode: String
    }

    struct Stopped: Decodable, Sendable {
        var stopped: Bool
    }

    struct List: DaemonRequest {
        typealias Response = Listed
        static let commandName: DaemonCommandName = "app-child-list"
        static let requiredCapability: String? = DaemonCapability.appChildren.rawValue
        var owner: String?
    }

    struct Listed: Decodable, Sendable {
        var children: [AppChildRow]
    }

    struct ReleaseAll: DaemonRequest {
        typealias Response = Released
        static let commandName: DaemonCommandName = "app-child-release-all"
        static let requiredCapability: String? = DaemonCapability.appChildren.rawValue
    }

    struct Released: Decodable, Sendable {
        var stopped: Int
    }
}
