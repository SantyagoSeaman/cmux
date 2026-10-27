import CmuxNextControl
import Dispatch
import Foundation

// cmux-next-headless: the app's model and control layers with no window (plans/cmux-next/chrome-parity/p15-oracle.md
// section H, task 160), for the refactor oracle's `--product headless` (task 161) and the CLI on a Linux VM.
//
//   cmux-next-headless --socket /tmp/o/h/control.sock --daemon-socket "$(cmux-tui --session S --json server ensure | jq -r .socket)"
//   cmux --socket /tmp/o/h/control.sock tree
//
// It prints one JSON line, `{"pid":N,"socket":PATH}`, once the socket accepts, and serves until SIGINT or SIGTERM.

/// Serves until a stop signal; returns the exit status.
@MainActor
private func serve(arguments: [String], environment: [String: String]) async -> Int32 {
    if arguments.contains(where: { $0 == "--help" || $0 == "-h" }) {
        print(HeadlessOptions.usage)
        return 0
    }
    let options: HeadlessOptions
    do {
        options = try HeadlessOptions(arguments: arguments, environment: environment)
    } catch {
        FileHandle.standardError.write(Data("cmux-next-headless: \(error)\n\(HeadlessOptions.usage)\n".utf8))
        return 64
    }
    // The stop signals as sources on the main queue (this process has no input to keep it from): the default
    // handlers would end the process before the socket file and the daemon connection close.
    let stop = AsyncStream.makeStream(of: Int32.self, bufferingPolicy: .bufferingNewest(1))
    let sources = [SIGINT, SIGTERM].map { number in
        signal(number, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: number, queue: .main)
        source.setEventHandler { [continuation = stop.continuation] in continuation.yield(number) }
        source.resume()
        return source
    }
    defer { sources.forEach { $0.cancel() } }
    let services: HeadlessServices
    do {
        let identity = ControlIdentity(appName: "cmux-next-headless", version: "0", build: "0", bundleID: nil, tag: nil,
                                       processID: getpid())
        services = try await HeadlessServices.start(options: options, identity: identity)
    } catch {
        FileHandle.standardError.write(Data("cmux-next-headless: \(error)\n".utf8))
        return 1
    }
    let ready: [String: Any] = ["socket": services.control.socketPath, "pid": Int(getpid())]
    let line = (try? JSONSerialization.data(withJSONObject: ready, options: [.sortedKeys])) ?? Data()
    FileHandle.standardOutput.write(line + Data("\n".utf8))
    for await _ in stop.stream { break }
    await services.stop()
    return 0
}

exit(await serve(arguments: Array(CommandLine.arguments.dropFirst()), environment: ProcessInfo.processInfo.environment))
