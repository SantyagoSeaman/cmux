/// The terminal's settings: its font (`terminal.fontFamily`, `terminal.fontSize`), echo
/// prediction (`terminal.predictiveEcho`) and cursor blink timeout (`terminal.cursorBlinkTimeout`).
nonisolated extension CmuxConfigSnapshot {
    mutating func parseTerminal(_ root: JSONValue) {
        let font = TerminalFontSetting()
        take(font.parseFamily(root), \.terminalFontFamily)
        take(font.parseSize(root), \.terminalFontSize)
        take(TerminalPredictiveEchoSetting.parse(root), \.terminalPredictiveEcho)
        take(TerminalCursorBlinkTimeoutSetting().parse(root), \.terminalCursorBlinkTimeout)
    }
}
