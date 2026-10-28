import AppKit
@testable import CmuxNextBrowser
import Testing

/// DIAG ONLY (never merged): the steady open's cost with and without flattened rows.
@MainActor @Suite(.serialized) struct ExtensionsMenuLayerDiagTests {
    static func layers(_ layer: CALayer?) -> Int {
        guard let layer else { return 0 }
        return 1 + (layer.sublayers ?? []).reduce(0) { $0 + layers($1) }
    }

    @Test(.enabled("a bubble needs a screen to show") { await ExtensionsMenuPerformanceTests.hasDisplay() })
    func steadyOpenByRowLayering() throws {
        typealias T = ExtensionsMenuPerformanceTests
        for round in 0..<3 {
            for flatten in [false, true] {
                ExtensionsMenuItemView.flattens = flatten
                defer { ExtensionsMenuItemView.flattens = false }
                let tab = T.tab()
                let (window, anchor) = T.window()
                defer { window.close() }
                let handler = ExtensionsMenu.DirectHandler(host: tab) { _ in }
                let controller = ExtensionsMenuController()
                let prepare = T.mainThreadCPU { controller.prepare(host: tab) }
                let first = T.mainThreadCPU { controller.show(host: tab, handler: handler, anchor: anchor) }
                try #require(controller.isShown)
                let count = Self.layers(controller.content.window?.contentView?.layer)
                controller.close()
                var opens: [Double] = [], closes: [Double] = []
                for _ in 0..<7 {
                    opens.append(T.milliseconds(T.mainThreadCPU { controller.show(host: tab, handler: handler, anchor: anchor) }))
                    closes.append(T.milliseconds(T.mainThreadCPU { controller.close() }))
                }
                controller.show(host: tab, handler: handler, anchor: anchor)
                let enter = try #require(NSEvent.enterExitEvent(with: .mouseEntered, location: .zero, modifierFlags: [], timestamp: 0,
                                                                windowNumber: 0, context: nil, eventNumber: 0, trackingNumber: 0, userData: nil))
                let exit = try #require(NSEvent.enterExitEvent(with: .mouseExited, location: .zero, modifierFlags: [], timestamp: 0,
                                                               windowNumber: 0, context: nil, eventNumber: 0, trackingNumber: 0, userData: nil))
                var hovers: [Double] = []
                for row in controller.content.rows {
                    let rep = try #require(row.bitmapImageRepForCachingDisplay(in: row.bounds))
                    hovers.append(T.milliseconds(T.fastest(3) {
                        row.primary.mouseEntered(with: enter)
                        row.cacheDisplay(in: row.bounds, to: rep)
                        row.primary.mouseExited(with: exit)
                    }))
                }
                controller.close()
                print(String(format: "ROW-LAYERS round=%d flatten=%@ layers=%d prepare=%.2f first=%.2f open.min=%.2f open.med=%.2f close.min=%.2f hover.max=%.2f",
                             round, flatten ? "on" : "off", count, T.milliseconds(prepare), T.milliseconds(first),
                             opens.min() ?? 0, opens.sorted()[3], closes.min() ?? 0, hovers.max() ?? 0))
            }
        }
    }
}
