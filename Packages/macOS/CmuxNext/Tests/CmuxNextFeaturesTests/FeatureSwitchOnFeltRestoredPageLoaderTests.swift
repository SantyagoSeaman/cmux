import CmuxNextFeatures
import Testing

/// Feature switch policy (plans/cmux-next/feature-switch-policy.md, rule 2, the measurement window):
/// `restoredPageLoader` (p08-startup-090) is a felt-queue switch (handoff felt-queue.md), so it is on by default in
/// DEV and NIGHTLY, and so in dogfood builds, while the optimized dogfood build benches it; Release stays off. With
/// no passing bench in 7 days it goes off again (rule 3).
struct FeatureSwitchOnFeltRestoredPageLoaderTests {
    @Test func isOnInDevAndNightlyAndOffInRelease() throws {
        let feature = try #require(FeatureRegistry().feature(named: "restoredPageLoader"), "restoredPageLoader is not declared")
        #expect(feature.defaults == FeatureDefaults(dev: true, nightly: true, release: false), "\(feature.defaults)")
    }

    @Test func aBuildWithNoInputResolvesItFromItsChannel() throws {
        let registry = FeatureRegistry()
        let feature = try #require(registry.feature(named: "restoredPageLoader"), "restoredPageLoader is not declared")
        for channel in Feature.Channel.allCases {
            #expect(FeatureList(registry: registry, channel: channel).resolution(of: feature)
                        == Feature.Resolution(isEnabled: channel != .release, source: .channelDefault), "\(channel)")
        }
    }
}
