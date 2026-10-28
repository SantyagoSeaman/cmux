import CmuxNextCompat
import CmuxNextIPC
import CmuxNextWakeups
import Darwin
import Foundation
import Testing
@testable import CmuxNextDaemon

/// IPC-12 (plans/cmux-next/chrome-parity/p05-ipc.md, task 190): the real daemon is killed with SIGKILL while ten
/// terminals flood their views, every view a stream on the app's one control connection (attach-streams-v1, with
/// rings once terminal-output-ring-v1 is granted). Chrome's model for the same event is a renderer crash: the
/// browser keeps running, the pipe error ends every associated endpoint, and the new process gets fresh
/// endpoints (mojo/public/cpp/bindings/README.md, "Connection errors"; content/browser/renderer_host/
/// render_process_host_impl.cc `ProcessDied`). Here:
///
/// - the app side does not crash: every view ends with `connectionLost` (never `bad_message`) and no attach line
///   fails to decode;
/// - from the new daemon's `listen()` (the birth time of the socket inode it binds, cmux-tui server/listen.rs
///   `bind_session_socket`) to the READY of every reattached view is at most 100 ms; the daemon's own start,
///   kill to `listen()`, is printed apart (D7);
/// - every view shows its own terminal: the first screen and all output carry only its marker, and the shell's
///   counter goes on past the value seen before the kill (the host survived and was adopted, not restarted);
/// - a span held across the kill keeps its ring mapped, and the release of that last span unmaps it
///   (app-daemon-connection-DESIGN.md section 4.3).
@Suite(.enabled(if: RealBinary.isBranchBuild, "needs the same-tree cmux-tui (scripts/cmux-next/pin-cmux-tui.sh fetch)"),
       .liveDaemon, .timeLimit(.minutes(3)))
struct IpcDaemonCrashTests {
    static let views = 10
    /// IPC-12's bound, from the new daemon's `listen()` to the last view's READY.
    static let readyBound: UInt64 = 100_000_000
    /// Output each view reads before the kill, so the kill lands mid-flood.
    static let floodBytes = 256 * 1024
    static let ringCapability = "terminal-output-ring-v1"

    @Test func aKilledDaemonLeavesEveryFloodingViewOnItsOwnScreenWithinTheBound() async throws {
        let binary = try #require(RealBinary.url)
        let id = UUID().uuidString.prefix(8).lowercased()
        let root = URL(fileURLWithPath: "/tmp/cnd-ipc-crash-\(id)")
        defer { try? FileManager.default.removeItem(at: root) }
        let environment = LoginEnvironment.shared.daemonEnvironment(login: nil, base: ProcessInfo.processInfo.environment,
                                                                    overrides: [:])
        let launcher = DaemonLauncher(
            work: BackgroundWork(label: "test.daemon-launcher"),
            configuration: .init(binary: binary, session: "cnd-ipc-crash-\(id)", stateDirectory: root.appendingPathComponent("state")),
            environment: { environment })
        _ = try await launcher.ensure()
        var configuration = BranchDaemonHarness.configuration()
        configuration.terminalStreams = true
        configuration.wireUpgrades = DaemonWireUpgrades(frames: DaemonFrameSchema(table: .schema, identity: .built),
                                                        streams: true)
        // The connection reconnects through the launcher, as the app's does: its `server ensure` starts the next daemon.
        let connection = DaemonConnection(configuration: configuration, endpointProvider: launcher.endpointProvider)
        let views: [FloodView]
        let runs: [Task<Void, Never>]
        let first = try await connection.start()
        do {
            try #require(first.supports(TerminalAttachment.streamsCapability), "the daemon offers no attach-streams-v1")
            views = try await Self.floods(on: connection, in: root)
            runs = views.map { view in Task { await view.run(on: connection) } }
        } catch {
            await BranchDaemonHarness.shutDown(connection)
            throw error
        }
        let undecodable = TerminalAttachment.undecodableLines.load(ordering: .relaxed)

        let flooding = await Self.until(.seconds(30)) { views.allSatisfy { $0.flooding(Self.floodBytes) } }
        #expect(flooding, "not every view read its flood: \(views.map(\.summary))")
        let rings = first.supports(Self.ringCapability)
        // The day the daemon offers the ring (p05-ipc-111b binds it) this is not recorded, the test fails, and the
        // wrapper goes: the ring checks below then run for real.
        withKnownIssue("cmux-tui offers no terminal-output-ring-v1 yet: IpcTransports::BOUND lacks OutputRing") {
            try #require(rings)
        }
        var heldAddress: UInt?
        if rings {
            views[0].holdNextSpan()
            heldAddress = await Self.until(.seconds(10)) { views[0].heldAddress != nil } ? views[0].heldAddress : nil
            #expect(heldAddress.map(RingMapping.isReadOnlyShared) == true, "a ring view's output is not a span of the ring mapping")
        }

        let before = views.map(\.counter)
        let killed = Wall.nanos()
        kill(first.pid, SIGKILL)
        let back = await Self.until(.seconds(60)) {
            zip(views, before).allSatisfy { view, counter in view.screened(after: killed) && view.counted(past: counter) }
        }
        let socket = await connection.endpoint?.socketPath
        let next = await connection.identity

        #expect(back, "not every view came back on its own screen: \(views.map(\.summary))")
        #expect(next.map { $0.pid != first.pid } == true, "no new daemon took over")
        if let listened = socket.flatMap(Wall.birth(of:)), listened > killed {
            let routed = views.compactMap { $0.routed(after: killed) }.min()
            let ready = views.compactMap { $0.screen(after: killed) }
            let last = ready.max() ?? listened
            print("IpcDaemonCrashTests: kill -> listen \(Wall.ms(listened - killed)) ms (daemon start, D7); "
                  + "listen -> control ready \(routed.map { Wall.ms($0 &- listened) } ?? "-") ms; "
                  + "listen -> every READY \(Wall.ms(last &- listened)) ms over \(ready.count) views")
            #expect(ready.count == Self.views)
            #expect(last &- listened <= Self.readyBound,
                    "listen() to the last READY took \(Wall.ms(last &- listened)) ms: \(ready.map { Wall.ms($0 &- listened) })")
        } else {
            Issue.record("the new daemon's socket \(socket ?? "-") was not bound after the kill")
        }
        for view in views {
            #expect(view.foreignMarkers == 0, "view \(view.index) showed another terminal's output: \(view.summary)")
            #expect(view.badScreens == 0, "view \(view.index) got a screen without its own terminal: \(view.summary)")
            #expect(view.closes.allSatisfy(Self.endedByTheKill), "view \(view.index) ended otherwise: \(view.closes)")
        }
        #expect(TerminalAttachment.undecodableLines.load(ordering: .relaxed) == undecodable)

        if let heldAddress {
            #expect(RingMapping.isReadOnlyShared(heldAddress), "a held span lost its mapping when its stream ended")
            views[0].releaseHeldSpan()
            #expect(!RingMapping.isReadOnlyShared(heldAddress), "the ring stayed mapped after its last span was released")
        }

        for view in views { view.stop() }
        for run in runs { await run.value }
        await BranchDaemonHarness.shutDown(connection)
    }

    /// The kill ends a view with its connection; a view that fell behind a flood may also have lost its stream
    /// (`overflow`) and reattached, as the app's attach driver does. Never a protocol error.
    private static func endedByTheKill(_ reason: TerminalChannelCloseReason) -> Bool {
        switch reason {
        case .connectionLost(let detail): !detail.hasPrefix("bad_message")
        case .overflow, .detachedByClient: true
        case .surfaceGone: false
        }
    }

    /// One workspace with ``views`` terminals, each printing `F<nn>:<count>` lines as fast as its shell can.
    private static func floods(on connection: DaemonConnection, in root: URL) async throws -> [FloodView] {
        let workspace = try await connection.createWorkspace(name: "ipc-crash")
        var created: [(index: Int, surface: SurfaceID)] = []
        for index in 1...views {
            let marker = FloodView.marker(index)
            let terminal = try await connection.createTerminal(
                in: workspace.key, cwd: root.path,
                argv: ["/bin/sh", "-c", "i=0; while :; do i=$((i+1)); echo \"\(marker)$i\"; done"],
                size: FloodView.size)
            created.append((index, try #require(terminal.surface)))
        }
        let tabs = try await connection.listWorkspaces().workspaces.flatMap(\.screens).flatMap(\.panes).flatMap(\.tabs)
        return created.map { index, surface in
            FloodView(index: index, surface: surface, resource: tabs.first { $0.surface == surface }?.terminalResourceID)
        }
    }

    /// Test harness poll: true once `condition` holds, false after `limit`.
    private static func until(_ limit: Duration, _ condition: () -> Bool) async -> Bool {
        let deadline = ContinuousClock.now + limit
        while !condition() {
            guard ContinuousClock.now < deadline else { return false }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return true
    }
}

/// One terminal's view, attached as a stream and reattached on the connection's next epoch each time its stream
/// ends, as the app's attach driver does. It reads every byte and records what a crash could get wrong.
private final class FloodView: Sendable {
    static let size = CellSize(cols: 80, rows: 24)
    /// Attach failures in a row before the view gives up.
    static let attachRetries = 50

    let index: Int
    let surface: SurfaceID
    let resource: ResourceID?
    private let state = Mutex(State())

    struct State {
        var bytes = 0
        /// The shell's newest counter seen in live output.
        var counter: UInt64 = 0
        var foreignMarkers = 0
        /// Screens (READY or replay) whose bytes named no terminal or another one.
        var badScreens = 0
        /// Wall time of each screen.
        var screens: [UInt64] = []
        /// Wall time of each ready control connection a reattach found.
        var routes: [UInt64] = []
        var closes: [TerminalChannelCloseReason] = []
        var failures: [String] = []
        var current: TerminalAttachment?
        var stopped = false
        var wantsHold = false
        var held: Data?
    }

    init(index: Int, surface: SurfaceID, resource: ResourceID?) {
        self.index = index
        self.surface = surface
        self.resource = resource
    }

    /// `F07:` for terminal 7.
    static func marker(_ index: Int) -> String {
        "F" + (index < 10 ? "0" : "") + String(index) + ":"
    }

    var counter: UInt64 { state.withLock { $0.counter } }
    var foreignMarkers: Int { state.withLock { $0.foreignMarkers } }
    var badScreens: Int { state.withLock { $0.badScreens } }
    var closes: [TerminalChannelCloseReason] { state.withLock { $0.closes } }
    var summary: String {
        state.withLock {
            "#\(index): \($0.bytes) B, counter \($0.counter), screens \($0.screens.count), closes \($0.closes), "
                + "failures \($0.failures.suffix(3))"
        }
    }

    func flooding(_ bytes: Int) -> Bool { state.withLock { $0.bytes >= bytes && $0.counter > 0 } }
    func screen(after wall: UInt64) -> UInt64? { state.withLock { $0.screens.first { $0 > wall } } }
    func screened(after wall: UInt64) -> Bool { screen(after: wall) != nil }
    func routed(after wall: UInt64) -> UInt64? { state.withLock { $0.routes.first { $0 > wall } } }
    func counted(past value: UInt64) -> Bool { counter > value }

    /// Keeps the next output span longer than `Data`'s inline bytes until ``releaseHeldSpan()``.
    func holdNextSpan() { state.withLock { $0.wantsHold = true } }
    var heldAddress: UInt? {
        state.withLock { state in
            // SAFETY: the address of the held bytes is only compared with mappings, never dereferenced.
            state.held.map { held in unsafe held.withUnsafeBytes { unsafe UInt(bitPattern: $0.baseAddress) } }
        }
    }

    /// Drops the held span: the last reference to its ring when its stream ended.
    func releaseHeldSpan() {
        _ = state.withLock { $0.held.take() }
    }

    func stop() {
        let current = state.withLock { state in
            state.stopped = true
            return state.current
        }
        current?.detachNow()
    }

    private var stopped: Bool { state.withLock { $0.stopped } }

    /// Attaches, reads until the stream ends, and attaches again, until ``stop()``.
    func run(on connection: DaemonConnection) async {
        var failed = 0
        while !stopped, failed < Self.attachRetries {
            do {
                guard case .stream(let carrier) = try await TerminalStreamCarrier.route(for: connection) else {
                    note(failure: "the connection does not stream terminals")
                    return
                }
                let routed = Wall.nanos()
                state.withLock { $0.routes.append(routed) }
                let target = TerminalAttachment.Target(surface: surface, terminalResourceID: resource,
                                                       generation: carrier.identity.generation)
                let view = try await TerminalAttachment.attachStream(
                    on: carrier, target: target, size: Self.size, claimGeometry: false, snapshotVersion: nil,
                    localHistory: false, images: false, createIntent: 0)
                guard adopt(view) else {
                    view.detachNow()
                    return
                }
                failed = 0
                for await event in view.events {
                    guard receive(event) else { break }
                }
            } catch {
                failed += 1
                note(failure: String(describing: error))
                try? await Task.sleep(for: .milliseconds(10))  // test harness: a terminal still being adopted
            }
        }
    }

    private func adopt(_ view: TerminalAttachment) -> Bool {
        state.withLock { state in
            guard !state.stopped else { return false }
            state.current = view
            return true
        }
    }

    private func note(failure: String) {
        state.withLock { $0.failures.append(failure) }
    }

    /// One event of the view; false when it ended the stream.
    private func receive(_ event: TerminalChannelEvent) -> Bool {
        switch event {
        case .replay(let replay):
            screen(replay.data)
        case .snapshot(let frame) where frame.phase == .ready:
            screen(nil)  // GHOSTSNP, not text: the output after it shows whose screen it is
        case .output(let data, _):
            output(data)
        case .closed(let reason):
            state.withLock { $0.closes.append(reason) }
            return false
        default:
            break
        }
        return true
    }

    private func screen(_ bytes: Data?) {
        let at = Wall.nanos()
        let scan = bytes.map { MarkerScan(of: $0) }
        state.withLock { state in
            state.screens.append(at)
            guard let scan else { return }
            state.foreignMarkers += scan.markers.filter { $0 != index }.count
            if !scan.markers.contains(index) { state.badScreens += 1 }
        }
    }

    private func output(_ data: Data) {
        let scan = MarkerScan(of: data)
        state.withLock { state in
            state.bytes += data.count
            state.foreignMarkers += scan.markers.filter { $0 != index }.count
            for (marker, counter) in scan.counters where marker == index { state.counter = max(state.counter, counter) }
            if state.wantsHold, state.held == nil, data.count > 64 {
                state.held = data
                state.wantsHold = false
            }
        }
    }
}

/// The `F<nn>:<count>` lines of one chunk: one pass over its bytes. A line cut at the chunk's edge is skipped.
private struct MarkerScan {
    /// The terminal of every marker, in order.
    let markers: [Int]
    /// Each complete line's terminal and counter.
    let counters: [(Int, UInt64)]

    init(of data: Data) {
        var markers: [Int] = []
        var counters: [(Int, UInt64)] = []
        // SAFETY: reads the bytes `withUnsafeBytes` lends for the call, each index checked against their count.
        unsafe data.withUnsafeBytes { (buffer: UnsafeRawBufferPointer) in
            let bytes = unsafe buffer.bindMemory(to: UInt8.self)
            let count = bytes.count
            func byte(_ at: Int) -> UInt8? { at < count ? unsafe bytes[at] : nil }
            func digit(_ at: Int) -> UInt8? { byte(at).flatMap { $0 >= 0x30 && $0 <= 0x39 ? $0 - 0x30 : nil } }
            var at = 0
            while at < count {
                guard byte(at) == 0x46, let tens = digit(at + 1), let ones = digit(at + 2), byte(at + 3) == 0x3A else {
                    at += 1
                    continue
                }
                let marker = Int(tens) * 10 + Int(ones)
                markers.append(marker)
                var end = at + 4
                var value: UInt64 = 0
                while let next = digit(end) {
                    value = value &* 10 &+ UInt64(next)
                    end += 1
                }
                if end > at + 4, byte(end) == 0x0D || byte(end) == 0x0A { counters.append((marker, value)) }
                at = end
            }
        }
        self.markers = markers
        self.counters = counters
    }
}

/// Wall-clock nanoseconds, the clock a socket inode's birth time is in.
private enum Wall {
    static func nanos() -> UInt64 {
        var now = timespec()
        // SAFETY: clock_gettime writes one timespec into `now` during the call.
        _ = unsafe clock_gettime(CLOCK_REALTIME, &now)
        return UInt64(now.tv_sec) * 1_000_000_000 + UInt64(now.tv_nsec)
    }

    /// When the socket at `path` was bound: `bind(2)` makes its inode, and the daemon listens on it at once
    /// (`UnixListener::bind`).
    static func birth(of path: String) -> UInt64? {
        var info = stat()
        // SAFETY: stat writes one `stat` into `info` during the call.
        guard unsafe stat(path, &info) == 0 else { return nil }
        let born = info.st_birthtimespec
        return UInt64(born.tv_sec) * 1_000_000_000 + UInt64(born.tv_nsec)
    }

    static func ms(_ nanos: UInt64) -> String {
        String(format: "%.1f", Double(nanos) / 1_000_000)
    }
}

/// This process's mappings, read from the kernel: whether a span's bytes still lie in the ring's read-only shared
/// mapping, so the test sees the unmap itself rather than a counter that says so.
private enum RingMapping {
    static func isReadOnlyShared(_ address: UInt) -> Bool {
        var start = mach_vm_address_t(address)
        var size: mach_vm_size_t = 0
        var info = vm_region_basic_info_data_64_t()
        var count = mach_msg_type_number_t(MemoryLayout<vm_region_basic_info_data_64_t>.size / MemoryLayout<Int32>.size)
        var object: mach_port_t = 0
        // SAFETY: mach_vm_region writes at most `count` words of `info` and the address, size and object name
        // during the call; `info` is a basic-info record of that many words.
        let result = unsafe withUnsafeMutablePointer(to: &info) { record in
            unsafe record.withMemoryRebound(to: Int32.self, capacity: Int(count)) { words in
                unsafe mach_vm_region(mach_task_self_, &start, &size, vm_region_flavor_t(VM_REGION_BASIC_INFO_64), words,
                                      &count, &object)
            }
        }
        // mach_vm_region answers the first region at or above the address: it must contain it.
        guard result == KERN_SUCCESS, start <= UInt64(address), UInt64(address) < start + size else { return false }
        return info.shared != 0 && info.protection & VM_PROT_READ != 0 && info.protection & VM_PROT_WRITE == 0
    }
}
