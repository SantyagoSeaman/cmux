/// cmux's cursor blink timeout (`terminal.cursorBlinkTimeout` in cmux.json, while the
/// `terminalCursorBlinkTimeout` feature is on, held in ``GhosttyRuntime/cursorBlinkTimeout``), loaded after the
/// Ghostty config files like `fontOverride`: Ghostty's `cursor-style-blink-timeout`, which stops a blinking
/// cursor that saw no key or output for that long so a terminal at rest draws no frames.
extension GhosttyRuntime {
    /// The longest timeout written, in seconds (`TerminalCursorBlinkTimeoutSetting.range`).
    nonisolated static let cursorBlinkTimeoutLimit: Double = 3600

    /// Config lines for `seconds` (0: the blink never stops), in whole milliseconds. None when nil, out of
    /// range, or when the pinned GhosttyNextKit has no `cursor-style-blink-timeout` key (an unknown key is
    /// a config diagnostic).
    public nonisolated static func cursorBlinkTimeoutLines(_ seconds: Double?) -> [String] {
        guard hasCursorBlinkTimeout, let seconds, seconds.isFinite, (0...cursorBlinkTimeoutLimit).contains(seconds) else {
            return []
        }
        return ["cursor-style-blink-timeout = \(Int((seconds * 1000).rounded()))ms"]
    }

    /// Whether the pinned GhosttyNextKit reads `cursor-style-blink-timeout` (Package.swift's
    /// `ghosttyNextKitHasCursorBlinkTimeout`).
    public nonisolated static var hasCursorBlinkTimeout: Bool {
        #if GHOSTTY_CURSOR_BLINK_TIMEOUT
        return true
        #else
        return false
        #endif
    }
}
