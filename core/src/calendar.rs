//! Calendar utilities: Gregorian ↔ Julian Day Number and system clock JDN.

/// Convert a Gregorian date and UTC time to Julian Day Number.
///
/// Uses the standard Meeus algorithm (valid for all dates after 1582-10-15).
pub fn gregorian_to_jdn(year: i32, month: u32, day: u32, hour: f64) -> f64 {
    let (y, m) = if month <= 2 {
        (year as f64 - 1.0, month as f64 + 12.0)
    } else {
        (year as f64, month as f64)
    };
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (y + 4716.0)).floor()
        + (30.6001 * (m + 1.0)).floor()
        + day as f64
        + b
        - 1524.5
        + hour / 24.0
}

/// Julian Day Number for the current UTC moment (from the system clock).
pub fn current_jdn() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        / 86400.0
        + 2440587.5
}
