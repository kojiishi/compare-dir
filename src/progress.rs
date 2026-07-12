use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use std::io::{IsTerminal, stderr};
use std::path::Path;
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Mutex, Weak};
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
    pub(crate) fn with_file_and_size(num_files: u64, size: u64) -> Self {
        Self { size, num_files }
    }

    pub(crate) fn with_size(size: u64) -> Self {
        Self::with_file_and_size(1, size)
    }

    fn is_zero(&self) -> bool {
        self.num_files == 0 && self.size == 0
    }

    fn saturating_sub(&self, other: &ProgressValue) -> Self {
        Self {
            num_files: self.num_files.saturating_sub(other.num_files),
            size: self.size.saturating_sub(other.size),
        }
    }

    fn assert_valid_for_len(&self, len: &ProgressValue) {
        assert!(
            self.num_files <= len.num_files && self.size <= len.size,
            "pos: {self:?}, len: {len:?}"
        );
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
    is_finished: bool,
    len: Option<ProgressValue>,
    multi: Option<MultiProgress>,
    primary: Weak<Mutex<Progress>>,
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.is_finished {
            self._inc_remaining();
            self.finish();
        }
    }
}

impl Progress {
    pub fn none() -> Self {
        Self::none_with_primary(Weak::new())
    }

    fn none_with_primary(primary: Weak<Mutex<Progress>>) -> Self {
        Self {
            inner: None,
            multi: None,
            primary,
            ..Default::default()
        }
    }

    fn update_style(&self) {
        if let Some(inner) = &self.inner {
            let style = if self.len.is_some() {
                match self.use_bytes {
                    true => format!("{NORMAL_STYLE0}{NORMAL_STYLE1_SIZE}"),
                    false => format!("{NORMAL_STYLE0}{NORMAL_STYLE1_NUM}"),
                }
            } else {
                match self.use_bytes {
                    true => format!("{SPINNER_STYLE0}{SPINNER_STYLE1_SIZE}"),
                    false => format!("{SPINNER_STYLE0}{SPINNER_STYLE1_NUM}"),
                }
            };
            inner.set_style(ProgressStyle::with_template(&style).unwrap());
        }
    }

    fn update_position(&self) {
        if let Some(inner) = &self.inner {
            inner.set_position(match self.use_bytes {
                true => self.pos.size,
                false => self.pos.num_files,
            });
        }
    }

    fn update_length(&self) {
        if let Some(inner) = &self.inner
            && let Some(len) = self.len
        {
            inner.set_length(match self.use_bytes {
                true => len.size,
                false => len.num_files,
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
        self.assert_pos();
        self.update_position();
        if let Some(primary) = self.primary.upgrade() {
            primary.lock().unwrap().inc(amount);
        }
    }

    pub fn inc_size(&mut self, size: u64) {
        let amount = ProgressValue::with_file_and_size(0, size);
        self.inc(amount);
    }

    pub fn inc_file(&mut self, num_files: u64) {
        let amount = ProgressValue::with_file_and_size(num_files, 0);
        self.inc(amount);
    }

    fn _inc_remaining(&mut self) {
        self.assert_pos();
        if let Some(len) = self.len {
            let remaining = len.saturating_sub(&self.pos);
            if !remaining.is_zero() {
                log::debug!("remaining: {remaining:?}");
                self.pos = len;
                if let Some(primary) = self.primary.upgrade() {
                    primary.lock().unwrap().inc(remaining);
                }
            }
        }
    }

    pub fn set_length(&mut self, len: ProgressValue) {
        self.len = Some(len);
        self.update_style();
        self.update_length();
    }

    fn assert_pos(&self) {
        if let Some(len) = self.len {
            self.pos.assert_valid_for_len(&len);
        }
    }

    pub fn use_bytes(&mut self) {
        self.use_bytes = true;
        self.update_style();
        self.update_length();
        self.update_position();
    }

    pub fn finish(&mut self) {
        assert!(!self.is_finished);
        self.is_finished = true;
        if let Some(len) = self.len {
            assert_eq!(self.pos, len);
        }
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

#[derive(Clone)]
pub(crate) struct SharedProgress {
    inner: Arc<Mutex<Progress>>,
}

impl SharedProgress {
    pub fn none() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Progress::none())),
        }
    }

    pub fn set_message(&self, msg: impl Into<String>) {
        self.inner.lock().unwrap().set_message(msg);
    }

    pub fn inc(&self, amount: ProgressValue) {
        self.inner.lock().unwrap().inc(amount);
    }

    pub fn set_length(&self, len: ProgressValue) {
        self.inner.lock().unwrap().set_length(len);
    }

    pub fn use_bytes(&self) {
        self.inner.lock().unwrap().use_bytes();
    }

    pub fn finish(&self) {
        self.inner.lock().unwrap().finish();
    }

    pub fn suspend_for<F, R, S: IsTerminal>(&self, stream: S, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.inner.lock().unwrap().suspend_for(stream, f)
    }
}

#[derive(Debug)]
pub struct ProgressBuilder {
    multi: MultiProgress,
    pub is_enabled: bool,
    pub is_file_enabled: bool,
    is_propagate: AtomicBool,
    primary: Mutex<Weak<Mutex<Progress>>>,
}

impl Default for ProgressBuilder {
    fn default() -> Self {
        Self {
            multi: MultiProgress::default(),
            is_enabled: stderr().is_terminal(),
            is_file_enabled: false,
            is_propagate: AtomicBool::new(false),
            primary: Mutex::new(Weak::new()),
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

    pub(crate) fn is_propagate(&self) -> bool {
        self.is_propagate.load(atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_propagate(&self) {
        self.is_propagate.store(true, atomic::Ordering::Relaxed);
    }

    pub(crate) fn add_primary(&self) -> SharedProgress {
        if !self.is_enabled {
            return SharedProgress::none();
        }
        let inner = self.multi.add(ProgressBar::new_spinner());
        inner.enable_steady_tick(Duration::from_secs(1));
        let progress = Progress {
            inner: Some(inner),
            multi: Some(self.multi.clone()),
            primary: Weak::new(),
            ..Default::default()
        };
        progress.update_style();
        let shared = SharedProgress {
            inner: Arc::new(Mutex::new(progress)),
        };
        *self.primary.lock().unwrap() = Arc::downgrade(&shared.inner);
        shared
    }

    pub(crate) fn add_file(&self, path: &Path, file_size: u64) -> Progress {
        if !self.is_enabled {
            return Progress::none();
        }
        let primary = if self.is_propagate() {
            Weak::clone(&self.primary.lock().unwrap())
        } else {
            Weak::new()
        };
        if !self.is_file_enabled {
            return Progress::none_with_primary(primary);
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
            len: Some(ProgressValue::with_size(file_size)),
            primary,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propagate() {
        let builder = ProgressBuilder {
            is_enabled: true,
            is_file_enabled: false,
            ..Default::default()
        };
        builder.set_propagate();
        let _primary = builder.add_primary();

        // Add a file progress
        let file_path = Path::new("dummy.txt");
        let mut file_progress = builder.add_file(file_path, 100);

        // Initially primary position is 0
        {
            let prim = builder.primary.lock().unwrap().upgrade().unwrap();
            assert_eq!(prim.lock().unwrap().pos.size, 0);
        }

        // Increment file progress
        file_progress.inc(ProgressValue::with_size(10));

        // Primary progress should be incremented to 10
        {
            let prim = builder.primary.lock().unwrap().upgrade().unwrap();
            assert_eq!(prim.lock().unwrap().pos.size, 10);
            assert_eq!(prim.lock().unwrap().pos.num_files, 1);
        }
    }
}
