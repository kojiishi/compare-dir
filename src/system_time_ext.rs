use std::{
    cmp::Ordering,
    time::{Duration, SystemTime},
};

pub(crate) trait SystemTimeExt {
    /// Compare two time with 100ns tolerance.
    /// This tolerance is needed because Windows `Duration` is based on
    /// `FILETIME` and thus has 100ns resolution, while other platforms may have
    /// higher (e.g. 1ns) resolution.
    fn cmp_nearly(&self, other: SystemTime) -> Ordering;
    fn eq_nearly(&self, other: SystemTime) -> bool;
}

impl SystemTimeExt for SystemTime {
    fn cmp_nearly(&self, other: SystemTime) -> Ordering {
        let (diff, ordering) = if *self > other {
            // `duration_since()` shouldn't fail if we check the order.
            (self.duration_since(other).unwrap(), Ordering::Greater)
        } else {
            (other.duration_since(*self).unwrap(), Ordering::Less)
        };
        if diff < Duration::from_nanos(100) {
            return Ordering::Equal;
        }
        ordering
    }

    fn eq_nearly(&self, other: SystemTime) -> bool {
        self.cmp_nearly(other) == Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use std::ops::{Add, Sub};
    use std::time::UNIX_EPOCH;

    use super::*;

    #[test]
    fn cmp_nearly() {
        let time = UNIX_EPOCH + Duration::new(12345, 67890);

        // Lookup with exact time should work
        assert_eq!(time.cmp_nearly(time), Ordering::Equal);
        assert!(time.eq_nearly(time));

        // Lookup with time differing by less than 100ns should work
        let add = time.add(Duration::new(0, 10));
        let sub = time.sub(Duration::new(0, 90));
        assert_eq!(time.cmp_nearly(add), Ordering::Equal);
        assert_eq!(time.cmp_nearly(sub), Ordering::Equal);
        assert!(time.eq_nearly(add));
        assert!(time.eq_nearly(sub));

        // Lookup with time differing by 100ns or more should fail
        let add = time.add(Duration::new(0, 100));
        assert_eq!(time.cmp_nearly(add), Ordering::Less);
        assert!(!time.eq_nearly(add));
        let sub = time.sub(Duration::new(0, 100));
        assert_eq!(time.cmp_nearly(sub), Ordering::Greater);
        assert!(!time.eq_nearly(sub));
    }
}
