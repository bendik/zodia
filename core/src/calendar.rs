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

/// Convert a Julian Day Number to a proleptic Gregorian calendar date (year, month, day).
///
/// Uses the Meeus algorithm (valid for all dates after 1582-10-15).
pub fn jdn_to_gregorian(jdn: f64) -> (i32, u32, u32) {
    let z = (jdn + 0.5).floor() as i64;
    let a = if z < 2_299_161 {
        z
    } else {
        let alpha = ((z as f64 - 1_867_216.25) / 36_524.25).floor() as i64;
        z + 1 + alpha - alpha / 4
    };
    let b = a + 1524;
    let c = ((b as f64 - 122.1) / 365.25).floor() as i64;
    let d = (365.25 * c as f64).floor() as i64;
    let e = ((b - d) as f64 / 30.6001).floor() as i64;

    let day = (b - d - (30.6001 * e as f64).floor() as i64) as u32;
    let month = if e < 14 { (e - 1) as u32 } else { (e - 13) as u32 };
    let year = if month > 2 { (c - 4716) as i32 } else { (c - 4715) as i32 };
    (year, month, day)
}

/// Format a JDN as a short human-readable date, e.g. `"12 Apr 2026"`.
pub fn jdn_to_display_date(jdn: f64) -> String {
    let (year, month, day) = jdn_to_gregorian(jdn);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun",
        "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {} {}", day, MONTHS[(month - 1) as usize], year)
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
