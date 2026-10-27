import CmuxNextControl
import CmuxNextSettings
import Foundation

/// The headless process's launch arguments (`cmux-next-headless --socket PATH --daemon-socket PATH`).
///
/// The control socket is the one the CLI and the oracle runner talk to; the daemon socket is a running cmux-tui
/// owner's (`cmux-tui --session S --json server ensure` prints it). Without `--daemon-socket` the process serves a
/// topology whose daemon state is `failed`, never an empty tree that reads as connected.
struct HeadlessOptions: Sendable, Equatable {
    /// The control socket to bind: `--socket`, else ``LaunchIdentity/socketOverrideKey`` (`CMUX_NEXT_SOCKET_PATH`).
    var controlSocket: String
    /// The cmux-tui owner's socket to mirror: `--daemon-socket`.
    var daemonSocket: String?
    /// The settings file: `--config`, else ``CmuxConfigFile/defaultURL(home:environment:)``.
    var configFile: URL
    /// `--access-mode` (`automation`, `cmuxOnly`, `off`); nil resolves it as the app does
    /// (``ControlService/resolveAccessMode(explicit:environment:configured:)``).
    var accessMode: ControlAccessMode?
    /// The process environment the access mode is resolved from.
    var environment: [String: String]

    static let usage = """
        usage: cmux-next-headless --socket PATH [--daemon-socket PATH] [--config PATH] [--access-mode automation|cmuxOnly|off]
        """

    /// Parses `arguments` (without the executable's name).
    init(arguments: [String], environment: [String: String]) throws(HeadlessOptionsError) {
        var socket = environment[LaunchIdentity.socketOverrideKey].flatMap { $0.isEmpty ? nil : $0 }
        var daemonSocket: String?
        var config: URL?
        var accessMode: ControlAccessMode?
        var rest = arguments[...]
        while let flag = rest.popFirst() {
            guard ["--socket", "--daemon-socket", "--config", "--access-mode"].contains(flag) else { throw .unknownArgument(flag) }
            guard let value = rest.popFirst(), !value.isEmpty else { throw .missingValue(flag) }
            switch flag {
            case "--socket": socket = value
            case "--daemon-socket": daemonSocket = value
            case "--config": config = URL(fileURLWithPath: (value as NSString).expandingTildeInPath)
            default:
                guard let mode = ControlService.parseAccessMode(value) else { throw .badAccessMode(value) }
                accessMode = mode
            }
        }
        guard let socket else { throw .noSocket }
        self.controlSocket = socket
        self.daemonSocket = daemonSocket
        self.configFile = config ?? CmuxConfigFile.defaultURL(environment: environment)
        self.accessMode = accessMode
        self.environment = environment
    }
}

/// Why the launch arguments do not parse.
enum HeadlessOptionsError: Error, Equatable, CustomStringConvertible {
    case unknownArgument(String)
    case missingValue(String)
    case badAccessMode(String)
    case noSocket

    var description: String {
        switch self {
        case .unknownArgument(let argument): "unknown argument \(argument)"
        case .missingValue(let flag): "\(flag) needs a value"
        case .badAccessMode(let mode): "unknown access mode \(mode)"
        case .noSocket: "no control socket: pass --socket or set \(LaunchIdentity.socketOverrideKey)"
        }
    }
}
