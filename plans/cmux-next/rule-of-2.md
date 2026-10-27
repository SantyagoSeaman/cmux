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
| Another browser's bookmarks, history, open tabs (JSON, plist, SQLite, SNSS, mozLz4) | `CmuxNextBrowserImport` readers, in the app | Swift; SQLite (C) for databases; LZ4 through Compression (C) | **no**, until the service is on | foreign | **3 of 3** (SQLite, Compression) | the sandboxed import service (section 2): built, behind `browserImportSandboxedService` |
| Another browser's cookies (Chromium `Cookies` SQLite, Firefox `cookies.sqlite`, Safari `Cookies.binarycookies`) | `CmuxNextBrowserImport/Cookies`, in the app | Swift; SQLite (C) | no | foreign | **3 of 3** | section 2, phase 3: the app reads the Safe Storage key from the Keychain and sends the key with the file |
| Another browser's saved passwords (`Login Data` SQLite, Firefox `key4.db` + `logins.json`, NSS DER) | `CmuxNextBrowserImport/Passwords`, in the app | Swift; SQLite (C) | no | foreign | **3 of 3** | section 2, phase 3, as cookies; the decrypted values stay in `SecretBytes` on the app side only |
| Another browser's search engine and extensions (`Preferences` JSON, `Web Data` SQLite, Firefox `search.json.mozlz4`) | `CmuxNextBrowserImport/Parsers`, in the app | Swift; SQLite and Compression (C) | no | foreign | **3 of 3** | section 2, phase 2: these read several files of a profile folder; the request sends each file |
| Export archives and password CSV files the person picks | `ExportArchive.swift` (zip through Compression), `PasswordCSVReader.swift` | Swift; Compression (C) | no | foreign | 3 of 3 for zip, 2 of 3 for CSV | zip into the service with phase 2 |
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

- **App, `CmuxNextBrowserImport`** decides what to read: `ProfileParseJob` (the format and the file,
  from the registry and folder listings only; nothing is opened). It keeps the merge and every write
  (`BrowserImporter`, `ImportHistorySync`, `ImportedDataStore`).
- **The one parse**, `ProfileParseSpec.parse(at:)`: the same call in both processes, so the
  in-process result and the service's result are the same value.
- **`ProfileDataReading`**, the seam: `InProcessProfileReader` parses here;
  `BrowserImportServiceReader` (target `CmuxNextBrowserImportService`) opens the job's file and a
  database's `-wal` and `-journal` read-only, where the folder grant is held, and sends the
  descriptors and the spec over XPC. One connection per parse.
- **The service** (`CmuxNextBrowserImportServiceMain`, `Contents/XPCServices/com.cmuxterm.browser-import.xpc`):
  `BrowserImportServiceGate` exports `BrowserImportService`; `BrowserImportParseHandler` copies the
  descriptors into a private folder in its container, runs the parse, and replies.

### The wire

- Request: a binary property list of `ProfileParseSpec` (format, Arc profile directory, history
  limit). No path, no profile name.
- Files: `main`, and `wal` and `journal` for a database, as `FileHandle`s (XPC sends a duplicate of
  each descriptor).
- Reply: a binary property list of `BrowserImportServiceReply`: `.parsed(ProfileParseResult)` or
  `.failed(BrowserImportServiceFailure)`. The app decodes it as these types only; bytes that do not
  decode, or a value of another kind than the format reads, are refused (`badReply`,
  `ProfileParseMismatch`), as Mojo's `ReportBadMessage` refuses a bad child message.
- A service that crashes or is ended fails only the profile it was parsing
  (`BrowserImportServiceError.connection`); `BrowserImporter` reports it and imports the others.

### Entitlements

`Sources/CmuxNextBrowserImportService/Bundle/BrowserImportService.entitlements`:
`com.apple.security.app-sandbox` and `com.apple.security.files.user-selected.read-only`. No
network client or server entitlement, no other file entitlement: the service reads only the
descriptors it is sent. `Bundle/Info.plist` declares the XPC service (`XPC!`, `ServiceType`
`Application`).

### Phases

1. **Done (task 208):** the service skeleton (interface, wire, handler, gate, reader, main, bundle
   files), and the parsers of bookmarks (Chromium, Safari, Arc, Firefox), history (Chromium,
   Firefox, Safari) and open tabs (Chromium, Firefox) behind it. `BrowserImporter` and
   `ImportHistorySync` take a reader; Import from Browser (`AppOnboardingServices.runImport`) picks
   the service when the feature `browserImportSandboxedService` (default off everywhere) is on. A
   build without the service bundle parses in process and logs it.
2. **The other callers, then search engines, extensions and export archives:** Keep in Sync
   (`BrowserFolderAccess` makes `ImportHistorySync` with the default reader) and the bookmarks
   page's import (`BookmarkBrowserImport`, `BrowserImporter.readBookmarks`) still parse in process;
   they take the same reader. Search engines, extensions and export archives each read several
   files of a profile, so the request carries a list of named files; `BrowserImporter.read`'s
   remaining in-process cases go.
3. **Cookies and passwords:** the app reads the Safe Storage key (Keychain prompt stays in the app)
   and sends it with the database; the service decrypts and returns `ImportedCookie` values, and
   passwords as sealed rows the app opens into `SecretBytes`. `CookieImporter` and
   `PasswordImporter` take the reader.
4. **Bundling:** an Xcode phase compiles `CmuxNextBrowserImportServiceMain` with its package
   dependencies into `Contents/XPCServices/com.cmuxterm.browser-import.xpc` (the server helper's
   `scripts/cmux-next/bundle-server-helper.sh` is the model), and `scripts/sign-cmux-bundle.sh`
   signs it with the entitlements above, inside out, before the app.
5. **The app stops linking the readers:** once the feature is on by default, the readers that open
   SQLite and Compression move out of `CmuxNextBrowserImport` into a target only the service
   links, and `InProcessProfileReader` is deleted. Only then does the app no longer link SQLite for
   import (it still links SQLite for its own databases).

### Tests

`Tests/CmuxNextBrowserImportTests/BrowserImportServiceTests.swift` (gated by `CMUX_IMPORT_FUZZ=1`,
run on the fleet): an anonymous XPC listener in the test process exports the same gate, so
descriptors and replies cross a real connection. Every format reads the same through the service as
in process; task 025's damaged-profile fuzz runs against the service; a bad request, a reader
error, a bad reply and a build without the service each have a case.

## 3. The next candidate: VNC decode

`CmuxVNC` decodes rectangles from any VNC server the person connects to, and hands zlib and ZRLE
data to zlib in the app process. The same shape fits: a sandboxed decoder service that gets the
rectangle bytes (or the socket itself) and returns pixels in shared memory. A memory-safe Swift
inflate, shared with the remote fetch and the terminal snapshots, would remove the C parser instead
and is the smaller change; both are open.
