import CmuxNextBrowserImportService
import Foundation

// The sandboxed browser import service (plans/cmux-next/rule-of-2.md): parses another browser's
// files that the app opened for it and replies with the values. Its entitlements
// (Bundle/BrowserImportService.entitlements) give it the App Sandbox with no network and no file
// access beyond the descriptors it is sent.

let gate = BrowserImportServiceGate()
let listener = NSXPCListener.service()
listener.delegate = gate
// A service listener's resume never returns: it runs the service until launchd ends it.
listener.resume()
