/// Formats a byte count using 1024-based units with two decimals, matching
/// the application UI.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    const T: u64 = G * 1024;

    #[expect(clippy::cast_precision_loss, reason = "display-only approximation")]
    match bytes {
        b if b >= T => format!("{:.2} TB", b as f64 / T as f64),
        b if b >= G => format!("{:.2} GB", b as f64 / G as f64),
        b if b >= M => format!("{:.2} MB", b as f64 / M as f64),
        b if b >= K => format!("{:.2} KB", b as f64 / K as f64),
        b => format!("{b} B"),
    }
}

/// Items per second, for a log line that has to show whether a machine is slow.
///
/// An operation that finished inside a millisecond is measured as one millisecond, so the rate
/// is an upper bound rather than a division by zero.
#[must_use]
pub fn per_second(count: u64, ms: u64) -> u64 {
    let rate = u128::from(count) * 1000 / u128::from(ms.max(1));
    u64::try_from(rate).unwrap_or(u64::MAX)
}

/// Megabytes per second, on the same convention as [`per_second`].
#[must_use]
pub fn megabytes_per_second(bytes: u64, ms: u64) -> u64 {
    per_second(bytes, ms) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, megabytes_per_second, per_second};

    #[test]
    fn formats_each_magnitude() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KB");
        assert_eq!(human_bytes(1536), "1.50 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.00 GB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024 * 1024), "2.00 TB");
    }

    #[test]
    fn a_rate_over_no_measurable_time_is_bounded_not_infinite() {
        // The whole point: `ms == 0` must not divide by zero.
        assert_eq!(per_second(50, 0), 50_000);
        assert_eq!(per_second(0, 0), 0);
        assert_eq!(per_second(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn rates_are_per_second_of_elapsed_time() {
        assert_eq!(per_second(434, 1000), 434);
        assert_eq!(per_second(17_904, 41_210), 434);
        assert_eq!(megabytes_per_second(12_400_000_000, 41_210), 300);
        assert_eq!(megabytes_per_second(1_000, 1_000), 0);
    }
}
