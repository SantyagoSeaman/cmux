/// `terminal.cursorBlinkTimeout` in cmux.json (p14-energy-069): the seconds a blinking terminal cursor
/// blinks with no key, output or focus change before it stops and stays shown, so a terminal at rest draws no
/// frames. 0 never stops the blink. GTK's `gtk-cursor-blink-timeout` does the same for text cursors.
/// Applied as Ghostty's `cursor-style-blink-timeout` (`GhosttyRuntime.cursorBlinkTimeout`) only while the
/// `terminalCursorBlinkTimeout` feature is on.
public nonisolated struct TerminalCursorBlinkTimeoutSetting: Sendable {
    public let path = ["terminal", "cursorBlinkTimeout"]
    /// The default, in seconds.
    public let fallback: Double = 15
    /// The seconds the setting accepts; 0 never stops the blink.
    public let range: ClosedRange<Double> = 0...3600

    public init() {}

    /// A missing key is the default with no diagnostic; a value that is not a number of seconds in
    /// `range` is the default plus a diagnostic.
    func parse(_ root: JSONValue) -> (Double, SettingsDiagnostic?) {
        guard let value = root.value(at: path) else { return (fallback, nil) }
        guard let seconds = value.doubleValue, seconds.isFinite, range.contains(seconds) else {
            return (fallback, SettingsDiagnostic(kind: .invalidValue, path: path.joined(separator: "."),
                                                 message: "expected seconds from \(Int(range.lowerBound)) to \(Int(range.upperBound)); 0 never stops the blink"))
        }
        return (seconds, nil)
    }
}
