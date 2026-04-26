use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SubProgress {
    pub(crate) inner: Option<ProgressBar>,
    pub(crate) multi: Option<Arc<MultiProgress>>,
    pub(crate) is_main: bool,
}

impl SubProgress {
    pub fn none() -> Self {
        Self {
            inner: None,
            multi: None,
            is_main: false,
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        if let Some(inner) = &self.inner {
            inner.set_message(msg.into());
        }
    }

    pub fn inc(&self, amount: u64) {
        if let Some(inner) = &self.inner {
            inner.inc(amount);
        }
    }

    pub fn length(&self) -> Option<u64> {
        self.inner.as_ref().and_then(|inner| inner.length())
    }

    pub fn set_length(&self, len: u64) {
        if let Some(inner) = &self.inner {
            inner.set_length(len);
            if self.is_main && len > 0 {
                inner.set_style(
                    ProgressStyle::with_template(ProgressReporter::NORMAL_STYLE).unwrap(),
                );
            }
        }
    }

    pub fn finish(&self) {
        if let Some(inner) = &self.inner {
            inner.finish();
            if let Some(multi) = &self.multi {
                multi.remove(inner);
            }
        }
    }

    pub fn suspend<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        if let Some(multi) = &self.multi {
            multi.suspend(f)
        } else if let Some(inner) = &self.inner {
            inner.suspend(f)
        } else {
            f()
        }
    }
}

#[derive(Debug)]
pub struct ProgressReporter {
    multi: Arc<MultiProgress>,
}

impl ProgressReporter {
    pub const INITIAL_STYLE: &str = "[{elapsed_precise}] {spinner:.green} {pos:>7} {msg}";
    pub const NORMAL_STYLE: &str =
        "[{elapsed_precise}] {bar:40.cyan/blue} {percent}% {pos:>7}/{len:7} {msg}";
    pub const HASH_STYLE: &str =
        "[{elapsed_precise}] {bar:20.cyan/blue} {percent}% {bytes:>7}/{total_bytes:7} {msg}";

    pub fn new() -> Self {
        Self {
            multi: Arc::new(MultiProgress::new()),
        }
    }

    pub fn add_main_bar(&self) -> SubProgress {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(120));
        pb.set_style(ProgressStyle::with_template(Self::INITIAL_STYLE).unwrap());
        let sub = SubProgress {
            inner: Some(pb),
            multi: Some(self.multi.clone()),
            is_main: true,
        };
        let sub_clone = sub.inner.as_ref().unwrap().clone();
        self.multi.add(sub_clone);
        sub
    }

    pub fn add_file_bar(&self, len: u64) -> SubProgress {
        let pb = ProgressBar::new(len);
        pb.set_style(ProgressStyle::with_template(Self::HASH_STYLE).unwrap());
        let sub = SubProgress {
            inner: Some(pb),
            multi: Some(self.multi.clone()),
            is_main: false,
        };
        let sub_clone = sub.inner.as_ref().unwrap().clone();
        self.multi.add(sub_clone);
        sub
    }
}

impl Default for ProgressReporter {
    fn default() -> Self {
        Self::new()
    }
}
