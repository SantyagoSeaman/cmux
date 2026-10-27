import CmuxNextCompat
import Darwin

/// The JSON meta of a RING frame, `{capacity, base, layout}` (design section 3.2, kind 7).
struct TerminalRingGrant: Decodable, Sendable, Equatable {
    /// The only CMAR layout this reader knows.
    static let knownLayout: UInt32 = 1
    /// Largest data area a reader maps (design section 4.3).
    static let maxCapacity: UInt64 = 64 << 20

    /// Bytes of the data area.
    var capacity: UInt64
    /// The PTY offset of the first byte this ring held.
    var base: UInt64
    var layout: UInt32
}

/// A RING or RING_ADVANCE the stream cannot use: a protocol error that ends the connection, so the view
/// reattaches on the next one and never falls back silently (design section 4.3).
enum TerminalRingRefusal: Error, Sendable, Equatable {
    /// A RING frame came without its descriptor.
    case noDescriptor
    /// The RING meta does not decode.
    case badMeta
    case unknownLayout(UInt32)
    /// Zero, or over ``TerminalRingGrant/maxCapacity``.
    case badCapacity(UInt64)
    /// This view maps no rings (the app did not ask for `terminal-output-ring-v1`).
    case unsupported
    /// The mapping failed or the descriptor is not a read-only ring of the grant (the mapper's reason).
    case mapping(String)
    /// A RING_ADVANCE before any RING, or one with a payload.
    case strayAdvance

    /// The `bad_message` reason the connection closes with.
    var reason: String {
        switch self {
        case .noDescriptor: "ring_without_descriptor"
        case .badMeta: "ring_bad_meta"
        case .unknownLayout(let layout): "ring_layout_\(layout)"
        case .badCapacity(let capacity): "ring_capacity_\(capacity)"
        case .unsupported: "ring_not_negotiated"
        case .mapping(let reason): "ring_mapping_\(reason)"
        case .strayAdvance: "ring_advance_without_ring"
        }
    }
}

/// A descriptor that came with a frame (`SCM_RIGHTS`): closed when nobody takes it.
final class TerminalRingDescriptor: Sendable {
    private let fd: Atomic<Int32>

    init(_ fd: Int32) {
        self.fd = Atomic(fd)
    }

    /// The descriptor, now the caller's to close; nil when it was taken before.
    func take() -> Int32? {
        let taken = fd.exchange(-1, ordering: .acquiringAndReleasing)
        return taken >= 0 ? taken : nil
    }

    deinit {
        let left = fd.exchange(-1, ordering: .acquiringAndReleasing)
        if left >= 0 { Darwin.close(left) }
    }
}
