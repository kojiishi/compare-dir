use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use std::io::{IsTerminal, stderr};
use std::path::Path;
use std::time::Duration;

const SPINNER_STYLE: &str = "{elapsed_precise} {spinner:.green} {pos:>7} {msg}";
const NORMAL_STYLE: &str =
    "{elapsed_precise} +{eta:>3} {percent:>3}% {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}";
const FILE_STYLE: &str = "  {elapsed:>3} +{eta:>3} {percent:>3}% {bar:10.cyan/blue} {wide_msg}";

#[derive(Debug)]
pub(crate) struct Progress {
    inner: Option<ProgressBar>,
    multi: Option<MultiProgress>,
}

impl Progress {
    pub fn none() -> Self {
        Self {
            inner: None,
            multi: None,
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
            if inner.length().is_none() {
                inner.set_style(ProgressStyle::with_template(NORMAL_STYLE).unwrap());
            }
            inner.set_length(len);
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

    pub fn suspend_for<F, R, S: IsTerminal>(&self, stream: S, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        if !stream.is_terminal() {
            return f();
        }
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
pub struct ProgressBuilder {
    multi: MultiProgress,
    pub is_enabled: bool,
    pub is_file_enabled: bool,
}

impl Default for ProgressBuilder {
    fn default() -> Self {
        Self {
            multi: MultiProgress::default(),
            is_enabled: stderr().is_terminal(),
            is_file_enabled: false,
        }
    }
}

impl ProgressBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init_logger(&self, logger: env_logger::Logger) -> anyhow::Result<()> {
        let max_level = logger.filter();
        LogWrapper::new(self.multi.clone(), logger).try_init()?;
        log::set_max_level(max_level);
        Ok(())
    }

    pub(crate) fn add_spinner(&self) -> Progress {
        if !self.is_enabled {
            return Progress::none();
        }
        let progress = self.multi.add(ProgressBar::new_spinner());
        progress.enable_steady_tick(Duration::from_secs(1));
        progress.set_style(ProgressStyle::with_template(SPINNER_STYLE).unwrap());
        Progress {
            inner: Some(progress),
            multi: Some(self.multi.clone()),
        }
    }

    pub(crate) fn add_file(&self, path: &Path, file_size: u64) -> Progress {
        if !self.is_enabled || !self.is_file_enabled {
            return Progress::none();
        }
        let progress = self.multi.add(ProgressBar::new(file_size));
        progress.set_style(ProgressStyle::with_template(FILE_STYLE).unwrap());
        if let Some(parent) = path.parent()
            && let Some(file_name) = path.file_name()
        {
            progress.set_message(format!(
                "{} ({})",
                file_name.to_string_lossy(),
                parent.to_string_lossy()
            ));
        } else {
            progress.set_message(path.to_string_lossy().to_string());
        }
        Progress {
            inner: Some(progress),
            multi: Some(self.multi.clone()),
        }
    }
}
