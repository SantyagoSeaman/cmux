import AppKit
@testable import CmuxNextDesign
@testable import CmuxNextTerminal
import CmuxNextWakeups
import GhosttyNextKit
import Testing

/// ghostty-integration-DESIGN.md G7: a focused terminal at rest whose cursor is hidden (agent TUIs keep
/// DECTCEM off) changes no pixel on a cursor blink tick, so Ghostty must draw no frame for it. Each tick
/// used to update and draw a frame (about 1.7 a second per terminal).
@MainActor @Suite(.serialized) struct TerminalIdleCursorFrameTests {
    @Test func aFocusedTerminalWithAHiddenCursorDrawsNothingAtRest() async throws {
        let terminal = try await FocusedTerminal()
        defer { terminal.close() }
        let shown = try await terminal.blinkTicksUpdateFrames()

        terminal.io.send(.output(Data("\u{1b}[?25l".utf8)))
        try await Task.sleep(for: .milliseconds(700)) // test: the hide lands and its frame draws
        let (updates, frames) = try await terminal.rest(for: .seconds(3)) // test: five blink ticks
        #expect(updates == 0, "at rest with a hidden cursor: \(updates) frame updates (\(frames) frames) in 3 s; visible: \(shown) in 2 s")
    }

    /// p14-energy-068: with the cursor blink layer on (`terminalCursorBlinkLayer`), a focused terminal at rest
    /// whose cursor blinks wakes its renderer for no blink, and the render server runs the blink: an opacity
    /// animation on the cursor sublayer of the surface's layer. Red below a GhosttyNextKit pin with
    /// `ghostty_app_set_cursor_blink_layer` (the blink ticks still update frames).
    @Test func aFocusedBlinkingCursorOnItsLayerWakesNoRenderer() async throws {
        let terminal = try await FocusedTerminal()
        let blinkLayer = GhosttyCursorBlinkLayer(runtime: .shared)
        defer {
            blinkLayer.set(false)
            terminal.close()
        }
        let shown = try await terminal.blinkTicksUpdateFrames()

        let taken = blinkLayer.set(true)
        try await Task.sleep(for: .milliseconds(700)) // test: the cursor moves to its layer and that frame draws
        let (updates, frames) = try await terminal.rest(for: .seconds(3)) // test: five blink ticks
        #expect(updates == 0 && frames == 0,
                "blinking on its layer: \(updates) frame updates (\(frames) frames) in 3 s; on the timer: \(shown) in 2 s; libghostty took the switch: \(taken)")
        let blinks = terminal.session.surfaceView.layer?.sublayers?.contains { $0.animation(forKey: "ghosttyCursorBlink") != nil } ?? false
        #expect(blinks, "no blink animation on the surface's cursor layer")
    }
}

/// A terminal in an on-screen window that drew its first output, its surface focused the way a key window's
/// first responder is (the runner's window is not key).
@MainActor
private struct FocusedTerminal {
    let io = ScriptedTerminalIO()
    let session: TerminalSession
    let surface: ghostty_surface_t
    let window: NSWindow
    private let drewWhenOccluded: Bool

    init() async throws {
        _ = NSApplication.shared
        GhosttyRuntime.startedForTests()
        drewWhenOccluded = WindowDrawPolicy.drawsWhenOccluded
        WindowDrawPolicy.drawsWhenOccluded = true
        session = TerminalSession(io: io, mainWork: MainWorkScheduler(budget: MainFrameBudget()))
        window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 480, height: 240), styleMask: [.borderless],
                          backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        guard let surface = session.surfaceView.surface else {
            io.finish()
            WindowDrawPolicy.drawsWhenOccluded = drewWhenOccluded
            throw FocusedTerminalError.noSurface
        }
        self.surface = surface
        window.contentView = session.view
        window.orderFrontRegardless()
        session.view.layoutSubtreeIfNeeded()
        io.send(.output(Data("ready\r\n".utf8)))
        do {
            let drawnBy = ContinuousClock.now.advanced(by: .seconds(10))
            while ContinuousClock.now < drawnBy, session.surfaceView.presentedFrame == nil {
                try await Task.sleep(for: .milliseconds(20)) // test: waits for the first frame
            }
            guard session.surfaceView.presentedFrame != nil else { throw FocusedTerminalError.neverDrew }
            ghostty_surface_set_focus(surface, true)
            try await Task.sleep(for: .milliseconds(500)) // test: the output's own frames land
        } catch {
            close()
            throw error
        }
    }

    func close() {
        io.finish()
        window.close()
        WindowDrawPolicy.drawsWhenOccluded = drewWhenOccluded
    }

    /// Control: a visible blinking cursor redraws on blink ticks, so the ticks run in this setup. Returns the
    /// frame updates of 2 s.
    func blinkTicksUpdateFrames() async throws -> UInt64 {
        let start = ghostty_surface_render_counters(surface)
        try await Task.sleep(for: .seconds(2)) // test: the control window, three blink ticks
        let shown = ghostty_surface_render_counters(surface).frame_updates - start.frame_updates
        try #require(shown > 0, "no blink tick updated a visible cursor's frame: the setup does not run blink ticks")
        return shown
    }

    /// The frame updates and frames drawn over `duration` at rest.
    func rest(for duration: Duration) async throws -> (updates: UInt64, frames: UInt64) {
        let before = ghostty_surface_render_counters(surface)
        try await Task.sleep(for: duration)
        let after = ghostty_surface_render_counters(surface)
        return (after.frame_updates - before.frame_updates, after.frames - before.frames)
    }
}

private enum FocusedTerminalError: Error {
    case noSurface
    case neverDrew
}
