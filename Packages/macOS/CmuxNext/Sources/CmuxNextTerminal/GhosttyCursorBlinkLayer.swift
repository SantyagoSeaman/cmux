import GhosttyNextKit

/// Ghostty's cursor blink on the render server (`ghostty_app_set_cursor_blink_layer`, ghostty-next
/// p14-energy-068, feature `terminalCursorBlinkLayer`): every surface draws a focused blinking cursor on a
/// sublayer of its layer whose opacity a Core Animation keyframe animation blinks, so no renderer thread runs
/// a blink timer and a focused terminal at rest wakes its renderer for no blink. AppKit's text insertion
/// indicator blinks on the compositor the same way; Chromium repaints its caret from a timer
/// (`third_party/blink/renderer/core/editing/frame_caret.cc`), the wake this removes.
///
/// libghostty links statically, so whether the switch exists is fixed by the GhosttyNextKit pin: the
/// `GHOSTTY_CURSOR_BLINK_LAYER` condition (`ghosttyNextKitCapabilities` in Package.swift) comes with the pin
/// bump that brings it. Below it nothing is sent and the cursor blinks on Ghostty's timer as before.
@MainActor
public struct GhosttyCursorBlinkLayer {
    private let runtime: GhosttyRuntime

    public init(runtime: GhosttyRuntime) {
        self.runtime = runtime
    }

    /// Turns the layer blink on or off for every surface, current and future. Returns whether libghostty
    /// took it: false before the app starts, or for a pin without the switch.
    @discardableResult
    public func set(_ enabled: Bool) -> Bool {
        #if GHOSTTY_CURSOR_BLINK_LAYER
        guard let app = runtime.app else { return false }
        ghostty_app_set_cursor_blink_layer(app, enabled)
        return true
        #else
        return false
        #endif
    }
}
