import Foundation
import Testing
@testable import CmuxNextBrowser

/// p10-stability-164: a GPU crash loop ended cmux. Chromium's last GPU fallback
/// (`GpuDataManagerImplPrivate::FallBackToNextGpuMode`, content/browser/gpu/gpu_data_manager_impl_private.cc)
/// is `LOG(FATAL)` in the browser process, which is the app. The fork patch `cmux_gpu_unusable_no_fatal`
/// (manaflow-ai/cef `cp/p10-stability-164`) turns that step into GPU off only when the browser process has
/// `cmux-gpu-unusable-no-fatal`. The default-off feature `chromiumGpuUnusableNoFatal` sends it on every fork,
/// spelled the way the patch reads it; with the feature off the browser process keeps stock Chromium's step.
@Suite struct CEFGpuCrashLoopSwitchTests {
    private func switches(forkAPIVersion: Int32, gpuUnusableNoFatal: Bool) -> [String] {
        CEFSwitches.current(
            forkAPIVersion: forkAPIVersion,
            gpuUnusableNoFatal: gpuUnusableNoFatal,
            bundleIdentifier: "com.cmuxterm.app.next",
            environment: [:]
        ).arguments
    }

    @Test(arguments: [Int32(1), 27, 33, 42])
    func everyForkGetsTheSwitchOnceWithTheFeatureOn(forkAPIVersion: Int32) {
        let arguments = switches(forkAPIVersion: forkAPIVersion, gpuUnusableNoFatal: true)
        #expect(arguments.filter { $0 == "cmux-gpu-unusable-no-fatal" }.count == 1, "\(arguments)")
    }

    @Test(arguments: [Int32(0), 1, 27, 33, 42])
    func noForkGetsTheSwitchWithTheFeatureOff(forkAPIVersion: Int32) {
        let arguments = switches(forkAPIVersion: forkAPIVersion, gpuUnusableNoFatal: false)
        #expect(!arguments.contains(CEFSwitches.gpuUnusableNoFatal), "\(arguments)")
    }

    @Test func theFeatureIsOffByDefault() {
        let arguments = CEFSwitches.current(forkAPIVersion: 42, bundleIdentifier: "com.cmuxterm.app.next", environment: [:])
            .arguments
        #expect(!arguments.contains(CEFSwitches.gpuUnusableNoFatal), "\(arguments)")
    }

    @Test func theSwitchIsNamedAsThePatchReadsIt() {
        #expect(CEFSwitches.gpuUnusableNoFatal == "cmux-gpu-unusable-no-fatal")
    }

    @Test func stockCEFHasNoForkSwitch() {
        #expect(!switches(forkAPIVersion: 0, gpuUnusableNoFatal: true).contains(CEFSwitches.gpuUnusableNoFatal))
    }
}
