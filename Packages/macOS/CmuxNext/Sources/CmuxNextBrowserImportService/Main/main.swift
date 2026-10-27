import CmuxNextBrowserImportParsers
import Foundation

// The sandboxed browser import service (plans/cmux-next/rule-of-2.md): parses another browser's
// files that the app opened for it and replies with the values. The Xcode target
// `cmux-browser-import` builds it into Contents/XPCServices/cmux-browser-import.xpc, and
// scripts/cmux-next/sign-browser-import-service.sh signs it with Bundle/BrowserImportService.entitlements
// (the App Sandbox with no network and no file access beyond the descriptors it is sent).

let gate = BrowserImportServiceGate()
let listener = NSXPCListener.service()
listener.delegate = gate
// A service listener's resume never returns: it runs the service until launchd ends it.
listener.resume()
