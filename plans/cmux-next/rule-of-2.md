# Rule of 2: untrusted parsers in cmux

Owner: PL-stability (chrome-parity program p10-stability, task 208).

Chromium's rule (`docs/security/rule-of-2.md`): code may have at most two of these three properties:

1. It reads **untrusted input** (bytes that another party wrote: a web page, another app's files, a
   remote peer).
2. It is written in an **unsafe language** (C, C++, Objective-C, Zig with unchecked builds, or Swift
   or Rust in `unsafe` blocks).
3. It runs with **no sandbox** (in the app or daemon process, with the person's full rights).

When a parser has all three, Chromium moves it into a sandboxed utility process (the data decoder
service, `services/data_decoder/`, for JSON, images and XML), or rewrites it in a safe language.
cmux follows the same order: first a sandbox, then a safe-language rewrite where the parser is ours.

## 1. Inventory

Trust levels: **hostile** (anyone on the internet or the network), **foreign** (another app on this
Mac wrote it, and the person did not check it), **peer** (a device or machine the person paired),
**own** (cmux or the person wrote it).

| Input | Where it is parsed | Parser language | Sandbox | Trust | Rule of 2 | Next step |
| --- | --- | --- | --- | --- | --- | --- |
| Web pages (HTML, JS, CSS, images, media) | Chromium renderer (CEF), WebKit WebContent | C++ | yes (Chromium and WebKit renderer sandboxes) | hostile | 2 of 3 | none |
| Another browser's bookmarks, history, open tabs (JSON, plist, SQLite, SNSS, mozLz4) | `CmuxNextBrowserImportParsers`, in the sandboxed import service | Swift; SQLite (C) for databases; LZ4 through Compression (C) | **yes** (App Sandbox, read-only selected files, no network) | foreign | 2 of 3 | none: the App links no reader (section 2) |
| Another browser's cookies (Chromium `Cookies` SQLite, Firefox `cookies.sqlite`, Safari `Cookies.binarycookies`) | `CmuxNextBrowserImportParsers/Cookies`, in the service; Chromium values decrypted in the app | Swift; SQLite (C) | **yes** | foreign | 2 of 3 | none: the Safe Storage key never leaves the app |
| Another browser's saved passwords (`Login Data` SQLite, Firefox `key4.db` + `logins.json`) | `CmuxNextBrowserImportParsers/Passwords`, in the service; NSS DER and decryption in the app | Swift; SQLite (C) | **yes** | foreign | 2 of 3 | none: the key and the primary password stay in the app, the values in `SecretBytes` |
| Another browser's search engine and extensions (`Preferences` JSON, `Web Data` SQLite, Firefox `search.json.mozlz4`, `manifest.json`, `messages.json`) | `CmuxNextBrowserImportParsers`, in the service, one file per request | Swift; SQLite and Compression (C) | **yes** | foreign | 2 of 3 | none |
| Export archives and password CSV files the person picks | `ExportArchive.swift` (zip through Compression) in the service; `PasswordCSVReader.swift` in the app | Swift; Compression (C) | **yes** for zip, no for CSV | foreign | 2 of 3 | none |
| VNC framebuffer updates (RFB: raw, zlib, ZRLE, cursor) | `Packages/Shared/CmuxVNC/Sources/CmuxVNC/Decode`, in the app | Swift; zlib (C) for zlib and ZRLE | no | peer (any VNC server the person connects to) | **3 of 3** (zlib) | **the next candidate**: the same service shape (a sandboxed decoder that gets the socket bytes and returns rectangles), or a Swift inflate |
| Images in agent replies, Home attachments, backgrounds, icons, favicons | ImageIO through `CheckedImageSource` / `ImageIO+Blocking` (CmuxNextCompat), CmuxNextAgentPane, CmuxNextHomeEngine, CmuxNextDesign, CmuxNextFavicons | ImageIO (C, C++, Objective-C) | no | hostile (web, agent output) | **3 of 3** | a decode service like Chromium's data decoder; until then sizes are capped and decode runs off main |
| SVG in agent replies and Home diagrams | `AgentPaneSVGSanitizer` (XMLParser, libxml2), `SVGRasterizer` (CoreSVG through NSImage) | C, Objective-C | no | hostile (agent output) | **3 of 3** | the image decode service; the sanitizer stays as a second gate |
| Sparkle appcast | `CmuxNextUpdater/AppcastParser.swift` (XMLParser, libxml2) | C | no | hostile network, over HTTPS from our host | 3 of 3, mitigated: the update archive is signed (EdDSA) and checked before use | the decode service's XML entry |
| Android UI tree (`uiautomator dump`) | `CmuxNextPhone/Android/UIAutomatorTree.swift` (XMLParser) | C | no | peer (a device on USB or adb) | 3 of 3 | the decode service's XML entry |
| HTTP bodies the remote fetch inflates | `CmuxNextRemoteFetch/RemoteFetchBodyDecoding.swift` (zlib, Compression) | C | no | hostile | 3 of 3 | the decode service, or a Swift inflate |
| Terminal snapshots and attachments from a daemon | `CmuxNextDaemon/Terminal/TerminalSnapshotInflate.swift`, `TerminalAttachment+Decoding.swift` (zlib) | C | no | own (local daemon) or peer (remote machine) | 2 of 3 local, 3 of 3 remote | a Swift inflate shared with VNC and the remote fetch |
| Terminal output (VT sequences, Kitty graphics, OSC) | Ghostty VT parser (`ghostty`, `cmux-tui/crates/ghostty-vt`), `cmux-kitty-media` | Zig (ReleaseFast has no bounds checks), Rust | no | hostile (any program, any ssh host) | **3 of 3** for Ghostty | out of scope here: Ghostty's own fuzzing; a safe build of the parser is tracked by PL-ghostty |
| Daemon wire, socket clients, relay frames | cmux-tui (Rust), CmuxNextDaemon (Swift) | Rust, Swift | no | own, peer | 2 of 3 | BadMessage on both sides (p10-stability-178) |
| Phone and device hub frames, signed HTTP replies | CmuxNextMobileHost, CmuxNextDevices | Swift (JSONSerialization, Codable) | no | peer | 2 of 3 | fuzz 027 |
| cmux settings, keybindings (`cmux-next.json`) | CmuxNextSettings | Swift | no | own | 1 of 3 | fuzz 026 |
| cmux's own databases (history, Chief, CEF `Cookies` for the billing check) | CmuxNextHistory, CmuxNextHomeEngine, `BrowserProfileLinkSession` | SQLite (C) | no | own | 2 of 3 | none |

Swift code is memory safe except in `unsafe` blocks (strict memory safety, p10-stability-211); the
unsafe-language column names the C, C++ or Zig code a Swift parser hands the bytes to.

## 2. The sandboxed browser import service

Chrome imports another browser's profile in a utility process: `ExternalProcessImporterHost` starts
`ProfileImportImpl` (`chrome/utility/importer/profile_import_impl.cc`), which runs the importer and
sends the items back over Mojo; the browser process merges them
(`chrome/browser/importer/external_process_importer_client.cc`, `in_process_importer_bridge.cc`).
cmux does the same, and goes one step further: the service gets open files, not paths, so it runs
in the App Sandbox with no file access of its own (Chrome's data decoder takes bytes the same way).

### The split

- **App, `CmuxNextBrowserImport`** decides what to read and keeps the merge, every key and every
  write: `ProfileParseJob` (the format and the file, from the registry and folder listings only;
  nothing is opened), the values that cross (`ProfileParseResult`, `SealedCookies`, `SealedLogins`,
  `FirefoxKeyStore`, `ChromiumPreferences`, `ExportArchiveParts`), and the flows over them
  (`BrowserImporter`, `ImportHistorySync`, `CookieImporter`, `PasswordImporter`,
  `ProfileSearchEngine`, `ProfileExtensions`). It links no SQLite and no Compression.
- **`ProfileDataReading`**, the seam. The App's one implementation is `BrowserImportServiceReader`
  (target `CmuxNextBrowserImportService`): it opens the job's file and a database's `-wal` and
  `-journal` read-only, where the folder grant is held, and sends the descriptors and the spec over
  XPC, one connection per parse. Every App caller takes it: Import from Browser, the import cards,
  Keep in Sync, the bookmarks page's import, `browser.importCookies` and Import from Another
  Browser's export archives. There is no in-process fallback: a build without the service reports
  `notBundled` for the profile.
- **The service, `CmuxNextBrowserImportParsers`**: every reader (`ProfileParseSpec.parse(at:)`),
  `BrowserImportParseHandler`, which copies the descriptors into a private folder in its container
  and replies, and `BrowserImportServiceGate`. Only the Xcode target `cmux-browser-import` links it
  (`Contents/XPCServices/cmux-browser-import.xpc`, main at
  `Sources/CmuxNextBrowserImportService/Main/main.swift`). `InProcessProfileReader` runs the same
  parse in a test process.
- **Secrets stay in the app.** Chromium cookie and password values cross sealed and are decrypted
  with the Safe Storage key the app reads from the Keychain; Firefox's `key4.db` crosses sealed and
  is opened with the primary password in the app. Chrome splits the same way: its utility importer
  sends items, the browser process keeps the profile keys.

### The wire

- Request: a binary property list of `ProfileParseSpec` (format, Arc profile directory, history
  limit, a key: the `Web Data` GUID or an extension message name). No path, no profile name.
- Files: `main`, and `wal` and `journal` for a database, as `FileHandle`s (XPC sends a duplicate of
  each descriptor).
- Reply: a binary property list of `BrowserImportServiceReply`: `.parsed(ProfileParseResult)` or
  `.failed(BrowserImportServiceFailure)` (a cookie reader's `CookieImportError` crosses as itself).
- The App trusts no reply. It decodes the wire's types only (bytes that do not decode are
  `badReply`), then `ProfileParseResult.checked(for:)` applies the readers' invariants, as a Mojo
  typemap's `StructTraits::Read` checks a child's message (mojo/public/cpp/bindings/struct_traits.h,
  chrome/common/importer/profile_import_process_param_traits): another kind than the format reads,
  a negative counter or one past `Int32.max`, or more entries than a reader gives is refused
  (`badReply`); an address no reader gives (`javascript:`, `chrome:`) is dropped and counted, visit
  counts are clamped, history is cut to the job's limit, times out of years 1-9999 are dropped or
  cleared, search engines keep https templates that take the typed text. Sums of skip counters
  saturate. A reply from an honest reader comes back unchanged.
- A service that crashes or is ended fails only the profile it was parsing
  (`BrowserImportServiceError.connection`); `BrowserImporter` reports it and imports the others.

### Entitlements and bundling

`Sources/CmuxNextBrowserImportService/Bundle/BrowserImportService.entitlements`:
`com.apple.security.app-sandbox` and `com.apple.security.files.user-selected.read-only`. No
network client or server entitlement, no other file entitlement: the service reads only the
descriptors it is sent. `Bundle/Info.plist` declares the XPC service (`XPC!`, `ServiceType`
`Application`, `com.cmuxterm.browser-import`).

- The app target `cmux-next` depends on the XPC target `cmux-browser-import`, embeds it with the copy
  phase "Embed XPC Services" and signs it in the phase "Sign browser import service"
  (`scripts/cmux-next/sign-browser-import-service.sh`): the build's identity, or ad hoc, always with
  the entitlements above, so a DEV tag runs it sandboxed too.
- `scripts/sign-cmux-bundle.sh` (and `scripts/ci/rcodesign-sign-bundle.sh`) sign it again for
  Developer ID with the hardened runtime, inside out before the app, and
  `sign-browser-import-service.sh --check` fails the signing unless the service carries exactly
  those two entitlements.

### Phases

1. **Done (task 208):** the inventory, the service (interface, wire, reply checks, handler, gate,
   reader, Xcode target, signing), and every reader of another browser's profile behind it:
   bookmarks, history, open tabs, cookies, passwords, search engines, extensions, export archives.
   The App links none of them.
2. **Next:** VNC decode (section 3), then the image and XML decoders of the inventory.

### Tests

`Tests/CmuxNextBrowserImportTests/BrowserImportServiceTests.swift` (gated by `CMUX_IMPORT_FUZZ=1`,
run on the fleet): an anonymous XPC listener in the test process exports the same gate, so
descriptors and replies cross a real connection. Every format reads the same through the service as
in process; task 025's damaged-profile fuzz runs against the service (cookies and passwords
included); a hostile service's well-formed replies (`Int.max` and negative counters, `javascript:`
addresses, `Int.max` visits, pages past the limit) are refused or cleaned, never trap; a bad request,
a reader error, a cookie reader's typed error, a bad reply and a build without the service each have
a case. On a built app, `sign-browser-import-service.sh --check <app>` checks the bundle.

## 3. The next candidate: VNC decode

`CmuxVNC` decodes rectangles from any VNC server the person connects to, and hands zlib and ZRLE
data to zlib in the app process. The same shape fits: a sandboxed decoder service that gets the
rectangle bytes (or the socket itself) and returns pixels in shared memory. A memory-safe Swift
inflate, shared with the remote fetch and the terminal snapshots, would remove the C parser instead
and is the smaller change; both are open.
