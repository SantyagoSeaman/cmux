import AppKit
@testable import CmuxNextDesign
@testable import CmuxNextSettings
@testable import CmuxNextTerminal
import CmuxNextWakeups
import GhosttyNextKit
import Testing

/// ghostty-integration-DESIGN.md G7: a focused terminal at rest whose cursor is hidden (agent TUIs keep
/// DECTCEM off) changes no pixel on a cursor blink tick, so Ghostty must draw no frame for it. Each tick
/// used to update and draw a frame (about 1.7 a second per terminal).
@MainActor @Suite(.serialized) struct TerminalIdleCursorFrameTests {
    /// A focused terminal in an ordered-front window whose first output drew, with its blinking cursor shown.
    @MainActor private final class FocusedTerminal {
        let io: ScriptedTerminalIO
        let session: TerminalSession
        let surface: ghostty_surface_t
        let window: NSWindow
        private let drew: Bool

        init() async throws {
            _ = NSApplication.shared
            GhosttyRuntime.startedForTests()
            let io = ScriptedTerminalIO()
            let session = TerminalSession(io: io, mainWork: MainWorkScheduler(budget: MainFrameBudget()))
            let surface = try #require(session.surfaceView.surface, "libghostty created no surface")
            let window = NSWindow(contentRect: NSRect(x: 200, y: 200, width: 480, height: 240), styleMask: [.borderless],
                                  backing: .buffered, defer: false)
            (self.io, self.session, self.surface, self.window) = (io, session, surface, window)
            drew = WindowDrawPolicy.drawsWhenOccluded
            WindowDrawPolicy.drawsWhenOccluded = true
            do {
                try await show()
            } catch {
                finish()
                throw error
            }
        }

        private func show() async throws {
            window.isReleasedWhenClosed = false
            window.contentView = session.view
            window.orderFrontRegardless()
            session.view.layoutSubtreeIfNeeded()
            // The runner's window is not key: the surface is focused below the way a key window's first
            // responder is.
            io.send(.output(Data("ready\r\n".utf8)))
            let drawnBy = ContinuousClock.now.advanced(by: .seconds(10))
            while ContinuousClock.now < drawnBy, session.surfaceView.presentedFrame == nil {
                try await Task.sleep(for: .milliseconds(20)) // test: the first frame lands
            }
            try #require(session.surfaceView.presentedFrame != nil, "the terminal never drew")
            ghostty_surface_set_focus(surface, true)
            try await Task.sleep(for: .milliseconds(500)) // test: the output's own frames land
        }

        func finish() {
            io.finish()
            window.close()
            WindowDrawPolicy.drawsWhenOccluded = drew
        }

        /// Frame updates over `seconds` from now.
        func frameUpdates(over seconds: Int) async throws -> UInt64 {
            let start = ghostty_surface_render_counters(surface)
            try await Task.sleep(for: .seconds(seconds)) // test: the measured window
            return ghostty_surface_render_counters(surface).frame_updates - start.frame_updates
        }

        /// Applies the user's Ghostty config with `terminal.cursorBlinkTimeout` at `seconds` to this surface.
        func applyCursorBlinkTimeout(_ seconds: Double) throws {
            var inputs = GhosttyConfigInputs.current(theme: nil)
            inputs.cursorBlinkTimeout = seconds
            let built = try #require(inputs.buildConfig(), "libghostty built no config")
            defer { built.free() }
            let rejected = built.diagnostics.filter { $0.contains("cursor-style-blink-timeout") }
            try #require(rejected.isEmpty, "Ghostty rejected the timeout: \(rejected)")
            ghostty_surface_update_config(surface, built.config)
        }

        /// Types `a` (kVK_ANSI_A) into the surface; the scripted host echoes nothing.
        func typeKey() {
            var key = ghostty_input_key_s()
            key.action = GHOSTTY_ACTION_PRESS
            key.mods = GHOSTTY_MODS_NONE
            key.consumed_mods = GHOSTTY_MODS_NONE
            key.keycode = 0
            key.unshifted_codepoint = 0x61 // a
            key.composing = false
            "a".withCString { text in
                key.text = text
                _ = ghostty_surface_key(surface, key)
            }
        }
    }

    @Test func aFocusedTerminalWithAHiddenCursorDrawsNothingAtRest() async throws {
        let terminal = try await FocusedTerminal()
        defer { terminal.finish() }

        // Control: a visible blinking cursor redraws on blink ticks, so the ticks run in this setup.
        let shown = try await terminal.frameUpdates(over: 2) // three blink ticks
        try #require(shown > 0, "no blink tick updated a visible cursor's frame: the setup does not run blink ticks")

        terminal.io.send(.output(Data("\u{1b}[?25l".utf8)))
        try await Task.sleep(for: .milliseconds(700)) // test: the hide lands and its frame draws
        let updates = try await terminal.frameUpdates(over: 3) // five blink ticks
        #expect(updates == 0, "at rest with a hidden cursor: \(updates) frame updates in 3 s; visible: \(shown) in 2 s")
    }

    /// p14-energy-069: at `terminal.cursorBlinkTimeout`'s default (15 s) a focused, visible, blinking cursor stops
    /// blinking, so the terminal draws no frame from 16 s to 26 s at rest; a key starts the blink again.
    @Test(.enabled(if: GhosttyRuntime.hasCursorBlinkTimeout, "the pinned GhosttyNextKit has no cursor-style-blink-timeout"))
    func aBlinkingCursorAtRestStopsAtTheTimeoutAndAKeyRestartsIt() async throws {
        let terminal = try await FocusedTerminal()
        defer { terminal.finish() }
        let blinking = try await terminal.frameUpdates(over: 2) // three blink ticks
        try #require(blinking > 0, "no blink tick updated a visible cursor's frame: the setup does not run blink ticks")

        try terminal.applyCursorBlinkTimeout(TerminalCursorBlinkTimeoutSetting().fallback)
        try await Task.sleep(for: .seconds(16)) // test: the timeout runs out with no output or key
        let resting = try await terminal.frameUpdates(over: 10)
        #expect(resting == 0, "16 s to 26 s at rest: \(resting) frame updates; blinking: \(blinking) in 2 s")

        terminal.typeKey()
        try await Task.sleep(for: .milliseconds(700)) // test: the key's own frame lands
        let restarted = try await terminal.frameUpdates(over: 2)
        #expect(restarted > 0, "a key did not start the blink again: \(restarted) frame updates in the 2 s after it")
    }

    /// p14-energy-069: `terminal.cursorBlinkTimeout` 0 keeps today's blink, which never stops.
    @Test(.enabled(if: GhosttyRuntime.hasCursorBlinkTimeout, "the pinned GhosttyNextKit has no cursor-style-blink-timeout"))
    func aBlinkTimeoutOfZeroNeverStopsTheBlink() async throws {
        let terminal = try await FocusedTerminal()
        defer { terminal.finish() }
        try terminal.applyCursorBlinkTimeout(0)
        try await Task.sleep(for: .seconds(16)) // test: past the default timeout with no output or key
        let updates = try await terminal.frameUpdates(over: 2)
        #expect(updates > 0, "a timeout of 0 stopped the blink: \(updates) frame updates in 2 s at 16 s")
    }

    /// The setting reaches Ghostty as whole milliseconds, only within its range and only with a kit that reads the key.
    @Test func theBlinkTimeoutBecomesGhosttyConfigInMilliseconds() {
        let lines = { GhosttyRuntime.cursorBlinkTimeoutLines($0) }
        guard GhosttyRuntime.hasCursorBlinkTimeout else {
            #expect(lines(15).isEmpty, "a kit without cursor-style-blink-timeout got a line it would reject")
            return
        }
        #expect(lines(15) == ["cursor-style-blink-timeout = 15000ms"])
        #expect(lines(0) == ["cursor-style-blink-timeout = 0ms"])
        #expect(lines(2.5) == ["cursor-style-blink-timeout = 2500ms"])
        #expect(lines(nil).isEmpty)
        #expect(lines(-1).isEmpty)
        #expect(lines(.infinity).isEmpty)
        #expect(lines(3601).isEmpty)
    }

    /// `terminal.cursorBlinkTimeout` parses seconds in 0...3600; anything else is the 15 s default plus a diagnostic.
    @Test func theBlinkTimeoutSettingParsesSeconds() {
        struct Parsed: Equatable {
            let seconds: Double
            let diagnosed: Bool
        }
        let setting = TerminalCursorBlinkTimeoutSetting()
        func parse(_ value: CmuxNextSettings.JSONValue?) -> Parsed {
            let root: CmuxNextSettings.JSONValue = value.map { .object(["terminal": .object(["cursorBlinkTimeout": $0])]) } ?? .object([:])
            let (seconds, diagnostic) = setting.parse(root)
            return Parsed(seconds: seconds, diagnosed: diagnostic != nil)
        }
        #expect(parse(nil) == Parsed(seconds: 15, diagnosed: false))
        #expect(parse(.number(0)) == Parsed(seconds: 0, diagnosed: false))
        #expect(parse(.number(30)) == Parsed(seconds: 30, diagnosed: false))
        #expect(parse(.number(-1)) == Parsed(seconds: 15, diagnosed: true))
        #expect(parse(.number(3601)) == Parsed(seconds: 15, diagnosed: true))
        #expect(parse(.string("15s")) == Parsed(seconds: 15, diagnosed: true))
    }
}
