use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

pub struct ProgressReporter {
    progress: ProgressBar,
}

impl ProgressReporter {
    pub fn new() -> Self {
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(Duration::from_millis(120));
        progress.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] {spinner:.green} {pos:>7} {msg}")
                .unwrap(),
        );
        Self { progress }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        self.progress.set_message(msg.into());
    }

    pub fn inc(&self, amount: u64) {
        self.progress.inc(amount);
    }

    pub fn length(&self) -> Option<u64> {
        self.progress.length()
    }

    pub fn set_length(&self, len: u64) {
        self.progress.set_length(len);
        if len > 0 {
            self.progress.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {bar:40.cyan/blue} {percent}% {pos:>7}/{len:7} {msg}",
                )
                .unwrap(),
            );
        }
    }

    pub fn finish(&self) {
        self.progress.finish();
    }

    pub fn suspend<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.progress.suspend(f)
    }
}

impl Default for ProgressReporter {
    fn default() -> Self {
        Self::new()
    }
}
