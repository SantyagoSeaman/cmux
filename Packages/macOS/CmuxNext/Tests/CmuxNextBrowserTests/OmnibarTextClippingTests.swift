import AppKit
@testable import CmuxNextBrowser
import CmuxNextDesign
import Foundation
import Testing

/// No omnibar text is ever cut off (user, 2026-10-27, "file slightly cut
/// off here": the right edge of the "File" chip's "e" was clipped). The
/// chip's label and the address keep every glyph pixel, focused or not,
/// for http and file pages.
@MainActor
struct OmnibarTextClippingTests {
    /// The opaque glyph pixel box of `view`, in backing pixels from its top left.
    private func inkBox(_ view: NSView) -> (minX: Int, minY: Int, maxX: Int, maxY: Int, width: Int, height: Int)? {
        view.superview?.layoutSubtreeIfNeeded()
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return nil }
        view.cacheDisplay(in: view.bounds, to: rep)
        var box: (minX: Int, minY: Int, maxX: Int, maxY: Int)?
        for y in 0..<rep.pixelsHigh {
            for x in 0..<rep.pixelsWide where (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.1 {
                box = (min(box?.minX ?? x, x), min(box?.minY ?? y, y), max(box?.maxX ?? x, x), max(box?.maxY ?? y, y))
            }
        }
        return box.map { ($0.minX, $0.minY, $0.maxX, $0.maxY, rep.pixelsWide, rep.pixelsHigh) }
    }

    private func window(_ size: NSSize) -> NSWindow {
        NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.titled], backing: .buffered, defer: false)
    }

    // MARK: Chip label

    nonisolated static let labels: [PageInfoIndicator] = [
        PageInfoIndicator(symbol: PageInfoIndicator.fileSymbol, label: .file, isTriggerable: true),
        PageInfoIndicator(symbol: PageInfoIndicator.notSecureSymbol, label: .notSecure, isTriggerable: true),
        PageInfoIndicator(symbol: PageInfoIndicator.dangerousSymbol, label: .dangerous, tone: .danger, isTriggerable: true),
        PageInfoIndicator(symbol: PageInfoIndicator.productSymbol, label: .product, isTriggerable: true),
        PageInfoIndicator(symbol: PageInfoIndicator.searchSymbol, label: .keyword("Search Google"), isTriggerable: false),
    ]

    /// The chip's label draws every glyph pixel a free-standing label of the
    /// same text and font draws: its frame holds the text and its padding.
    @Test(arguments: labels)
    func chipLabelIsNeverClipped(_ indicator: PageInfoIndicator) throws {
        let chip = PageInfoChipButton()
        let window = window(NSSize(width: 400, height: 60))
        window.contentView!.addSubview(chip)
        chip.indicator = indicator
        chip.frame = NSRect(origin: NSPoint(x: 10, y: 10), size: chip.intrinsicContentSize)
        chip.layoutSubtreeIfNeeded()
        let label = try #require(chip.subviews.compactMap { $0 as? ChromeLabel }.first { !$0.isHidden })

        let free = ChromeLabel(labelWithString: label.stringValue)
        free.font = label.font
        free.sizeToFit()
        window.contentView!.addSubview(free)
        let reference = try #require(inkBox(free))
        let shown = try #require(inkBox(label))

        #expect(label.frame.width >= label.naturalSize.width,
                "\"\(label.stringValue)\": frame \(label.frame.width) pt, text with padding \(label.naturalSize.width) pt")
        #expect(shown.maxX - shown.minX == reference.maxX - reference.minX,
                "\"\(label.stringValue)\" ink width: shown \(shown.maxX - shown.minX + 1) px, whole text \(reference.maxX - reference.minX + 1) px")
        #expect(shown.maxX < shown.width - 1, "\"\(label.stringValue)\" ink reaches the label's right edge")
        #expect(label.frame.maxX <= chip.bounds.maxX, "the label stays inside the chip")
    }

    // MARK: Address text

    private func bar(_ url: URL, security: BrowserSecurityState) -> (AddressBarView, NSWindow) {
        let bar = AddressBarView()
        let window = window(NSSize(width: 900, height: 80))
        window.contentView!.addSubview(bar)
        NSLayoutConstraint.activate([
            bar.leadingAnchor.constraint(equalTo: window.contentView!.leadingAnchor, constant: 10),
            bar.widthAnchor.constraint(equalToConstant: 840),
            bar.topAnchor.constraint(equalTo: window.contentView!.topAnchor, constant: 20),
        ])
        bar.update(url: url, security: security)
        window.contentView!.layoutSubtreeIfNeeded()
        return (bar, window)
    }

    nonisolated static let addresses: [String] = [
        "https://github.com/manaflow-ai/cmuxterm-hq",
        "file:///var/folders/xy/T/report.html",
    ]

    /// The address keeps its top, bottom and left glyph pixels at rest and
    /// while editing.
    @Test(arguments: addresses)
    func addressIsNeverClipped(_ address: String) throws {
        let url = try #require(URL(string: address))
        let (bar, _) = bar(url, security: .secure)
        let rest = try #require(inkBox(bar.field))
        #expect(rest.minX > 0 && rest.minY > 0 && rest.maxY < rest.height - 1,
                "\(address) at rest: ink \(rest.minX),\(rest.minY)-\(rest.maxY) in \(rest.width)x\(rest.height) px")

        bar.focus()
        let editor = try #require(bar.field.currentEditor() as? NSTextView)
        editor.selectedTextAttributes = [:]
        editor.insertionPointColor = .clear
        editor.setSelectedRange(NSRange(location: 0, length: 0))
        let editing = try #require(inkBox(bar.field))
        #expect(editing.minX > 0 && editing.minY > 0 && editing.maxY < editing.height - 1,
                "\(address) editing: ink \(editing.minX),\(editing.minY)-\(editing.maxY) in \(editing.width)x\(editing.height) px")
    }
}
