import Foundation
import Testing
@testable import CmuxNextBrowser

/// p10-stability-164: a GPU crash loop ended cmux. Chromium's last GPU fallback
/// (`GpuDataManagerImplPrivate::FallBackToNextGpuMode`, content/browser/gpu/gpu_data_manager_impl_private.cc)
/// is `LOG(FATAL)` in the browser process, which is the app. The fork patch `cmux_gpu_unusable_no_fatal`
/// (manaflow-ai/cef `cp/p10-stability-164`) turns that step into GPU off only when the browser process has
/// `cmux-gpu-unusable-no-fatal`, so the switch must reach it on every fork, spelled the way the patch reads it.
@Suite struct CEFGpuCrashLoopSwitchTests {
    private func switches(forkAPIVersion: Int32) -> [String] {
        CEFSwitches.current(forkAPIVersion: forkAPIVersion, bundleIdentifier: "com.cmuxterm.app.next", environment: [:])
            .arguments
    }

    @Test(arguments: [Int32(1), 27, 33, 42])
    func everyForkGetsTheSwitchOnce(forkAPIVersion: Int32) {
        let arguments = switches(forkAPIVersion: forkAPIVersion)
        #expect(arguments.filter { $0 == "cmux-gpu-unusable-no-fatal" }.count == 1, "\(arguments)")
    }

    @Test func theSwitchIsNamedAsThePatchReadsIt() {
        #expect(CEFSwitches.gpuUnusableNoFatal == "cmux-gpu-unusable-no-fatal")
    }

    @Test func stockCEFHasNoForkSwitch() {
        #expect(!switches(forkAPIVersion: 0).contains(CEFSwitches.gpuUnusableNoFatal))
    }
}
