import CmuxNextActions
import CmuxNextControl
import CmuxNextSettings
import CmuxNextWakeups

/// The catalog actions the headless process runs: the settings choices that need no window (animation kinds,
/// motion categories, chrome style, tab overflow, link routing). Each applies to the settings controller's design
/// at once and writes cmux.json through the controller's one validated path, as the app's appearance handlers do
/// (CmuxNextApp/Handlers/AppearanceHandlers.swift, AppActionContext+Settings.swift).
@MainActor
struct HeadlessSettingsActions {
    let registry: ActionRegistry
    let settings: SettingsController
    let snapshots: ControlSnapshotStore
    let mainWork: MainWorkScheduler

    func bind() {
        let save = makeSave()
        AnimationKindsSetting.bindActions(into: registry, design: settings.design, save: save)
        MotionCategoriesSetting.bindActions(into: registry, design: settings.design, save: save)
        ChromeStyleSetting.bindActions(into: registry, design: settings.design, save: save)
        TabOverflowSetting.bindActions(into: registry, design: settings.design, save: save)
        LinkRouting.bindActions(into: registry, save: save)
    }

    /// Refuses a managed key before anything changes; otherwise starts the write as the run's tracked work, so
    /// `action.run` answers after it. Its main-thread end, an automation task for a socket run, marks the control
    /// snapshot stale: the next read builds the snapshot from the reloaded settings, whatever publish ran while the
    /// write was in flight.
    private func makeSave() -> SettingChoiceActions.Save {
        let (registry, settings, snapshots, mainWork) = (registry, settings, snapshots, mainWork)
        return { [weak registry] path, value in
            if let managed = settings.managedKey(forPath: path) {
                return SettingManaged(key: managed.key, source: managed.source).description
            }
            // The run's origin, read now (the write runs later): a socket run may change only agent-settable keys.
            let writer = SettingWriter.currentRun()
            registry?.trackWork(owner: "headless.settings.write", on: mainWork) { () async -> String? in
                do {
                    try await settings.setSetting(at: path, to: value, by: writer)
                    return nil
                } catch {
                    // The design already changed for this session: the file's value applies again.
                    await settings.reload()
                    return String(describing: error)
                }
            } then: { result in
                snapshots.markStale()
                guard case .success(let failure?) = result else { return nil }
                return ActionWorkFailure(failure)
            }
            return nil
        }
    }
}
