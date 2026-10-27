import AppKit
@testable import CmuxNextDesign
@testable import CmuxNextTerminal
import CmuxNextWakeups
import GhosttyNextKit
import IOSurface
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
    /// whose cursor blinks wakes its renderer thread 0 times in 10 s, and the render server runs the blink: an
    /// opacity animation on the cursor sublayer of the surface's layer. The control, the same terminal with the
    /// layer off, is the red state: its blink ticks wake the renderer and update frames.
    @Test(.enabled(if: GhosttyCursorBlinkLayer.isAvailable, "needs a GhosttyNextKit pin with ghostty_app_set_cursor_blink_layer (p14-energy-068a)"))
    func aFocusedBlinkingCursorOnItsLayerWakesNoRenderer() async throws {
        let terminal = try await FocusedTerminal()
        let blinkLayer = GhosttyCursorBlinkLayer(runtime: .shared)
        defer {
            blinkLayer.set(false)
            terminal.close()
        }
        let control = try await terminal.rest(countersFor: .seconds(2)) // test: the control window, three blink ticks
        try #require(control.frameUpdates > 0 && (control.cursorBlinkTicks ?? 0) > 0,
                     "with the layer off: \(control); the setup runs no blink ticks, or Ghostty does not count them")

        try #require(blinkLayer.set(true), "libghostty did not take the switch")
        try await Task.sleep(for: .milliseconds(700)) // test: the cursor moves to its layer and that frame draws
        let rest = try await terminal.rest(countersFor: .seconds(10)) // test: the acceptance window, 16 blink ticks off
        #expect(rest.rendererWakes == 0 && rest.cursorBlinkTicks == 0 && rest.frameUpdates == 0 && rest.frames == 0,
                "blinking on its layer, 10 s at rest: \(rest); with the layer off, 2 s: \(control)")
        let layer = try #require(terminal.session.surfaceView.layer)
        #expect(CursorSublayer(in: layer) != nil, "no blink animation on the surface's cursor layer")
    }

    /// p14-energy-068: a resize with the layer on keeps the blinking cursor on its cell in the commit of the new size.
    /// The display pass that presents the frame at the new size presents that frame's cursor with it; one that keeps
    /// the last frame (not drawn within 4 ms) keeps the cursor at its distance from the top-left corner, where the
    /// contents stay. Narrower wraps the line, so the new frame's cursor is on another cell; taller moves a sublayer
    /// placed from the bottom edge by 160 points.
    @Test(.enabled(if: GhosttyCursorBlinkLayer.isAvailable, "needs a GhosttyNextKit pin with ghostty_app_set_cursor_blink_layer (p14-energy-068a)"))
    func aResizeKeepsTheBlinkingCursorOnItsCell() async throws {
        let terminal = try await FocusedTerminal()
        let blinkLayer = GhosttyCursorBlinkLayer(runtime: .shared)
        defer {
            blinkLayer.set(false)
            terminal.close()
        }
        try #require(blinkLayer.set(true), "libghostty did not take the switch")
        terminal.io.send(.output(Data(String(repeating: "x", count: 40).utf8)))
        try await Task.sleep(for: .milliseconds(700)) // test: the line, then the cursor's layer, land
        let layer = try #require(terminal.session.surfaceView.layer)
        let before = try #require(CursorSublayer(in: layer), "no cursor sublayer with the layer on")
        let sizeBefore = try #require(Self.contentsSize(of: layer))

        terminal.window.setContentSize(NSSize(width: 240, height: 400))
        terminal.session.view.layoutSubtreeIfNeeded()
        // The display pass of the new size, in this main-thread turn: no main-queue present can run before the reads.
        _ = ghostty_surface_present_host_frame(terminal.surface)
        layer.layoutIfNeeded()

        let after = try #require(CursorSublayer(in: layer), "the cursor sublayer lost its blink")
        #expect(!after.layer.isHidden)
        let size = try #require(Self.contentsSize(of: layer))
        if size != sizeBefore {
            let cell = terminal.cursorCellCenter()
            #expect(after.fromTopLeft.contains(cell),
                    "the frame at the new size has its cursor at \(cell), the cursor sublayer is at \(after.fromTopLeft) (before: \(before.fromTopLeft))")
        } else {
            #expect(abs(after.fromTopLeft.minX - before.fromTopLeft.minX) < 0.5
                        && abs(after.fromTopLeft.minY - before.fromTopLeft.minY) < 0.5,
                    "the last frame stays at the top-left corner, its cursor moved from \(before.fromTopLeft) to \(after.fromTopLeft)")
        }
    }

    /// The pixel size of the IOSurface the layer shows.
    private static func contentsSize(of layer: CALayer) -> [Int]? {
        guard let contents = layer.contents else { return nil }
        let object = contents as CFTypeRef
        guard CFGetTypeID(object) == IOSurfaceGetTypeID() else { return nil }
        let surface = unsafeDowncast(object, to: IOSurface.self)
        return [surface.width, surface.height]
    }
}

/// The cursor sublayer of a surface's layer (the one the blink animates), and its frame from the layer's top-left
/// corner, in points.
@MainActor
private struct CursorSublayer {
    let layer: CALayer
    let fromTopLeft: CGRect

    init?(in surfaceLayer: CALayer) {
        guard let layer = surfaceLayer.sublayers?.first(where: { $0.animation(forKey: "ghosttyCursorBlink") != nil }) else { return nil }
        self.layer = layer
        let bounds = surfaceLayer.bounds
        let frame = layer.frame
        let top = surfaceLayer.isGeometryFlipped ? frame.minY - bounds.minY : bounds.maxY - frame.maxY
        fromTopLeft = CGRect(x: frame.minX - bounds.minX, y: top, width: frame.width, height: frame.height)
    }
}

/// Ghostty's counters for one surface over a window at rest.
private struct RestCounts: CustomStringConvertible {
    var frameUpdates: UInt64
    var frames: UInt64
    /// Nil below a GhosttyNextKit with them (`TerminalRenderCounters/rendererWakes`).
    var rendererWakes: UInt64?
    var cursorBlinkTicks: UInt64?

    init(from before: TerminalRenderCounters, to after: TerminalRenderCounters) {
        frameUpdates = after.frameUpdates - before.frameUpdates
        frames = after.frames - before.frames
        rendererWakes = before.rendererWakes.flatMap { start in after.rendererWakes.map { $0 - start } }
        cursorBlinkTicks = before.cursorBlinkTicks.flatMap { start in after.cursorBlinkTicks.map { $0 - start } }
    }

    var description: String {
        "\(frameUpdates) frame updates, \(frames) frames, \(rendererWakes.map(String.init) ?? "?") renderer wakes, "
            + "\(cursorBlinkTicks.map(String.init) ?? "?") blink ticks"
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

    /// Ghostty's counters (``TerminalRenderCounters``, the `debug.surfaces` report's) over `duration` at rest.
    func rest(countersFor duration: Duration) async throws -> RestCounts {
        let before = try #require(session.diagnostics.renderCounters)
        try await Task.sleep(for: duration)
        let after = try #require(session.diagnostics.renderCounters)
        return RestCounts(from: before, to: after)
    }

    /// The center of the cursor's cell from the surface's top-left corner, in points: `ghostty_surface_ime_point`
    /// gives the cell's horizontal middle, its bottom edge and its height.
    func cursorCellCenter() -> CGPoint {
        var x = 0.0, y = 0.0, width = 0.0, height = 0.0
        ghostty_surface_ime_point(surface, &x, &y, &width, &height)
        return CGPoint(x: x, y: y - height / 2)
    }
}

private enum FocusedTerminalError: Error {
    case noSurface
    case neverDrew
}
