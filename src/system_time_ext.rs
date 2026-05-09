use std::time::{Duration, SystemTime};

pub(crate) trait SystemTimeExt {
    /// Returns true if the time is within 100ns tolerance.
    /// This tolerance is needed because Windows `Duration` is based on
    /// `FILETIME` and thus has 100ns resolution, while other platforms may have
    /// higher (e.g. 1ns) resolution.
    fn eq_nearly(&self, other: SystemTime) -> bool;
}

impl SystemTimeExt for SystemTime {
    fn eq_nearly(&self, other: SystemTime) -> bool {
        let diff = if other > *self {
            // `duration_since()` shouldn't fail if we check the order.
            other.duration_since(*self).unwrap()
        } else {
            self.duration_since(other).unwrap()
        };
        diff < Duration::from_nanos(100)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::{Add, Sub};
    use std::time::UNIX_EPOCH;

    use super::*;

    #[test]
    fn test_eq_nearly() {
        let time = UNIX_EPOCH + Duration::new(12345, 67890);

        // Lookup with exact time should work
        assert!(time.eq_nearly(time));

        // Lookup with time differing by less than 100ns should work
        assert!(time.eq_nearly(time.add(Duration::new(0, 10))));
        assert!(time.eq_nearly(time.sub(Duration::new(0, 90))));

        // Lookup with time differing by 100ns or more should fail
        assert!(!time.eq_nearly(time.add(Duration::new(0, 100))));
        assert!(!time.eq_nearly(time.sub(Duration::new(0, 100))));
    }
}
