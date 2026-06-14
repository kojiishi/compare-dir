use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use std::io::{IsTerminal, stderr};
use std::path::Path;
use std::time::Duration;

const SPINNER_STYLE0: &str = "{elapsed_precise} {spinner:.green} ";
const SPINNER_STYLE1_NUM: &str = "{pos:>7} {msg}";
const SPINNER_STYLE1_SIZE: &str = "{bytes:>7} {msg}";
const NORMAL_STYLE0: &str = "{elapsed_precise} +{eta:>3} {percent:>3}% {bar:40.cyan/blue} ";
const NORMAL_STYLE1_NUM: &str = "{pos:>7}/{len:7} {msg}";
const NORMAL_STYLE1_SIZE: &str = "{bytes:>7}/{total_bytes:7} {msg}";
const FILE_STYLE: &str = "  {elapsed:>3} +{eta:>3} {percent:>3}% {bar:10.cyan/blue} {wide_msg}";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProgressValue {
    pub(crate) num_files: u64,
    size: u64,
}

impl ProgressValue {
    pub(crate) fn with_size(size: u64) -> Self {
        Self { size, num_files: 1 }
    }

    pub(crate) fn with_skip(size: u64) -> Self {
        Self { size, num_files: 1 }
    }
}

impl std::ops::Add for ProgressValue {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            num_files: self.num_files + other.num_files,
            size: self.size + other.size,
        }
    }
}

impl std::ops::AddAssign for ProgressValue {
    fn add_assign(&mut self, other: Self) {
        self.num_files += other.num_files;
        self.size += other.size;
    }
}

#[derive(Debug, Default)]
pub(crate) struct Progress {
    inner: Option<ProgressBar>,
    pos: ProgressValue,
    use_bytes: bool,
    len: Option<ProgressValue>,
    multi: Option<MultiProgress>,
}

impl Progress {
    pub fn none() -> Self {
        Self {
            inner: None,
            multi: None,
            ..Default::default()
        }
    }

    fn update_style(&self) {
        if let Some(inner) = &self.inner {
            let style = if self.len.is_some() {
                if self.use_bytes {
                    format!("{NORMAL_STYLE0}{NORMAL_STYLE1_SIZE}")
                } else {
                    format!("{NORMAL_STYLE0}{NORMAL_STYLE1_NUM}")
                }
            } else {
                if self.use_bytes {
                    format!("{SPINNER_STYLE0}{SPINNER_STYLE1_SIZE}")
                } else {
                    format!("{SPINNER_STYLE0}{SPINNER_STYLE1_NUM}")
                }
            };
            inner.set_style(ProgressStyle::with_template(&style).unwrap());
        }
    }

    fn update_position(&self) {
        if let Some(inner) = &self.inner {
            inner.set_position(if self.use_bytes {
                self.pos.size
            } else {
                self.pos.num_files
            });
        }
    }

    fn update_length(&self) {
        if let Some(inner) = &self.inner
            && let Some(len) = self.len
        {
            inner.set_length(if self.use_bytes {
                len.size
            } else {
                len.num_files
            });
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        if let Some(inner) = &self.inner {
            inner.set_message(msg.into());
        }
    }

    pub fn inc(&mut self, amount: ProgressValue) {
        self.pos += amount;
        self.update_position();
    }

    pub fn set_length(&mut self, len: ProgressValue) {
        self.len = Some(len);
        self.update_style();
        self.update_length();
    }

    pub fn use_bytes(&mut self) {
        self.use_bytes = true;
        self.update_style();
        self.update_length();
        self.update_position();
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
        let inner = self.multi.add(ProgressBar::new_spinner());
        inner.enable_steady_tick(Duration::from_secs(1));
        let progress = Progress {
            inner: Some(inner),
            multi: Some(self.multi.clone()),
            ..Default::default()
        };
        progress.update_style();
        progress
    }

    pub(crate) fn add_file(&self, path: &Path, file_size: u64) -> Progress {
        if !self.is_enabled || !self.is_file_enabled {
            return Progress::none();
        }
        let inner = self.multi.add(ProgressBar::new(file_size));
        inner.set_style(ProgressStyle::with_template(FILE_STYLE).unwrap());
        if let Some(parent) = path.parent()
            && let Some(file_name) = path.file_name()
        {
            inner.set_message(format!(
                "{} ({})",
                file_name.to_string_lossy(),
                parent.to_string_lossy()
            ));
        } else {
            inner.set_message(path.to_string_lossy().to_string());
        }
        Progress {
            inner: Some(inner),
            multi: Some(self.multi.clone()),
            use_bytes: true,
            ..Default::default()
        }
    }
}
