use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum StartupLoadStage {
    #[default]
    Idle,
    DiscoveringRollouts,
    LoadingRolloutCache,
    ParsingRollouts,
    SavingRolloutCache,
    ReducingRollouts,
    MaterializingSnapshot,
    LoadingHistory,
    Complete,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartupLoadProgress {
    pub(crate) revision: u64,
    pub(crate) active: bool,
    pub(crate) stage: StartupLoadStage,
    pub(crate) completed_files: usize,
    pub(crate) total_files: usize,
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: u64,
    pub(crate) current_file_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StartupLoadProgressTracker {
    inner: Arc<Mutex<StartupLoadProgress>>,
}

impl StartupLoadProgressTracker {
    pub(crate) fn snapshot(&self) -> StartupLoadProgress {
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn begin(&self) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            let revision = progress.revision.wrapping_add(1);
            *progress = StartupLoadProgress {
                revision,
                active: true,
                stage: StartupLoadStage::DiscoveringRollouts,
                ..StartupLoadProgress::default()
            };
        });
    }

    pub(crate) fn set_rollout_totals(
        &self,
        stage: StartupLoadStage,
        total_files: usize,
        total_bytes: u64,
    ) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            progress.active = true;
            progress.stage = stage;
            progress.completed_files = 0;
            progress.total_files = total_files;
            progress.completed_bytes = 0;
            progress.total_bytes = total_bytes;
            progress.current_file_bytes = 0;
        });
    }

    pub(crate) fn set_stage(&self, stage: StartupLoadStage) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            progress.active = true;
            progress.stage = stage;
            progress.current_file_bytes = 0;
        });
    }

    pub(crate) fn processed_files(
        &self,
        stage: StartupLoadStage,
        completed_files: usize,
        completed_bytes: u64,
    ) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            progress.active = true;
            progress.stage = stage;
            progress.completed_files = completed_files.min(progress.total_files);
            progress.completed_bytes = completed_bytes.min(progress.total_bytes);
            progress.current_file_bytes = 0;
        });
    }

    pub(crate) fn parsing_file(
        &self,
        completed_files: usize,
        completed_bytes: u64,
        current_file_bytes: u64,
    ) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            progress.active = true;
            progress.stage = StartupLoadStage::ParsingRollouts;
            progress.completed_files = completed_files.min(progress.total_files);
            progress.completed_bytes = completed_bytes.min(progress.total_bytes);
            progress.current_file_bytes = current_file_bytes;
        });
    }

    pub(crate) fn parsed_file(&self, completed_files: usize, completed_bytes: u64) {
        self.update(|progress| {
            if progress.stage == StartupLoadStage::Complete {
                return;
            }
            progress.active = true;
            progress.stage = StartupLoadStage::ParsingRollouts;
            progress.completed_files = completed_files.min(progress.total_files);
            progress.completed_bytes = completed_bytes.min(progress.total_bytes);
            progress.current_file_bytes = 0;
        });
    }

    pub(crate) fn finish(&self) {
        self.update(|progress| {
            progress.active = false;
            progress.stage = StartupLoadStage::Complete;
            progress.completed_files = progress.total_files;
            progress.completed_bytes = progress.total_bytes;
            progress.current_file_bytes = 0;
        });
    }

    fn update(&self, update: impl FnOnce(&mut StartupLoadProgress)) {
        let mut progress = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = *progress;
        update(&mut progress);
        if *progress != previous {
            progress.revision = previous.revision.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_reports_monotonic_bounded_rollout_progress() {
        let tracker = StartupLoadProgressTracker::default();
        tracker.begin();
        tracker.set_rollout_totals(StartupLoadStage::LoadingRolloutCache, 2, 300);
        tracker.set_stage(StartupLoadStage::ParsingRollouts);
        tracker.parsing_file(0, 0, 100);
        tracker.parsed_file(1, 100);
        tracker.parsing_file(1, 100, 200);
        tracker.parsed_file(2, 300);

        let progress = tracker.snapshot();
        assert!(progress.active);
        assert_eq!(progress.stage, StartupLoadStage::ParsingRollouts);
        assert_eq!(progress.completed_files, 2);
        assert_eq!(progress.total_files, 2);
        assert_eq!(progress.completed_bytes, 300);
        assert_eq!(progress.total_bytes, 300);
        assert_eq!(progress.current_file_bytes, 0);

        tracker.finish();
        let completed = tracker.snapshot();
        assert!(!completed.active);
        assert_eq!(completed.stage, StartupLoadStage::Complete);
        assert!(completed.revision > progress.revision);
    }

    #[test]
    fn tracker_clamps_untrusted_progress_to_discovered_totals() {
        let tracker = StartupLoadProgressTracker::default();
        tracker.begin();
        tracker.set_rollout_totals(StartupLoadStage::ParsingRollouts, 1, 10);
        tracker.parsed_file(usize::MAX, u64::MAX);

        let progress = tracker.snapshot();
        assert_eq!(progress.completed_files, 1);
        assert_eq!(progress.completed_bytes, 10);
    }

    #[test]
    fn completed_startup_tracker_cannot_be_revived_by_later_scans() {
        let tracker = StartupLoadProgressTracker::default();
        tracker.begin();
        tracker.set_rollout_totals(StartupLoadStage::ParsingRollouts, 2, 20);
        tracker.finish();
        let completed = tracker.snapshot();

        tracker.begin();
        tracker.set_rollout_totals(StartupLoadStage::LoadingRolloutCache, 9, 90);
        tracker.parsed_file(9, 90);

        assert_eq!(tracker.snapshot(), completed);
    }
}
