// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Hyper-specific types: Numeric, Date, Time, Timestamp, Interval, Geography.
//!
//! These types match the encoding used by Hyper's internal format.

#![allow(
    clippy::cast_precision_loss,
    reason = "type conversion for fractional-second math; f64 precision matches input"
)]

use bytes::{BufMut, BytesMut};
use std::error::Error;
use std::fmt;

use super::traits::{
    write_not_null_indicator, FromHyperBinary, ToHyperBinary, NULL_INDICATOR_SIZE,
};

// =============================================================================
// Date - Days since 2000-01-01 (Hyper epoch)
// =============================================================================

/// A date value (days since 2000-01-01).
///
/// Hyper uses 2000-01-01 as the epoch, which is different from Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Date {
    /// Days since 2000-01-01 (can be negative for dates before epoch)
    days: i32,
}

impl Date {
    /// The Hyper epoch year: 2000
    pub const EPOCH_YEAR: i32 = 2000;
    /// The Hyper epoch month: 1 (January)
    pub const EPOCH_MONTH: u32 = 1;
    /// The Hyper epoch day: 1
    pub const EPOCH_DAY: u32 = 1;
    /// Julian Day Number for 2000-01-01 (the Hyper epoch)
    pub const JULIAN_DAY_EPOCH: i32 = 2451545;

    /// Minimum supported days offset (year 1, Jan 1 relative to 2000-01-01).
    pub const MIN_DAYS: i32 = Self::MIN_JULIAN_DAY - Self::JULIAN_DAY_EPOCH;
    /// Maximum supported days offset (year 9999, Dec 31 relative to 2000-01-01).
    pub const MAX_DAYS: i32 = Self::MAX_JULIAN_DAY - Self::JULIAN_DAY_EPOCH;

    /// Creates a Date from days since epoch.
    ///
    /// No validation is performed. For a checked variant, use [`try_from_days`](Self::try_from_days).
    pub const fn from_days(days: i32) -> Self {
        Date { days }
    }

    /// Creates a Date from days since epoch with range validation.
    ///
    /// # Errors
    ///
    /// Returns an error if `days` falls outside the supported range
    /// (years 1-9999, i.e. [`MIN_DAYS`](Self::MIN_DAYS)..=[`MAX_DAYS`](Self::MAX_DAYS)).
    pub fn try_from_days(days: i32) -> std::result::Result<Self, Box<dyn Error + Send + Sync>> {
        if !(Self::MIN_DAYS..=Self::MAX_DAYS).contains(&days) {
            return Err(format!(
                "days offset {days} is out of supported range ({} to {}, years 1-9999)",
                Self::MIN_DAYS,
                Self::MAX_DAYS,
            )
            .into());
        }
        Ok(Date { days })
    }

    /// Creates a Date from year, month, day components.
    ///
    /// # Panics
    ///
    /// Panics if the date is invalid.
    pub fn new(year: i32, month: u32, day: u32) -> Self {
        let days = Self::ymd_to_days(year, month, day);
        Date { days }
    }

    /// Returns the days since epoch.
    pub const fn days(&self) -> i32 {
        self.days
    }

    /// Returns the year, month, day components.
    pub fn to_ymd(&self) -> (i32, u32, u32) {
        Self::days_to_ymd(self.days)
    }

    /// Returns the raw encoded value for HyperBinary (Julian Day Number).
    ///
    /// Uses wrapping arithmetic to avoid panic for out-of-range days values.
    /// Callers should validate dates are within supported range (years 1-9999)
    /// before relying on the encoded value.
    pub const fn encode(&self) -> u32 {
        // Bit-pattern reinterpret: Hyper wire format encodes Julian day as a u32.
        // `wrapping_add` handles i32 overflow; the `as u32` then reinterprets the
        // resulting bit pattern. No data loss — this is the inverse of `decode`.
        #[expect(
            clippy::cast_sign_loss,
            reason = "intentional u32 bit-pattern reinterpret; inverse of Date::decode"
        )]
        let encoded = self.days.wrapping_add(Self::JULIAN_DAY_EPOCH) as u32;
        encoded
    }

    /// Creates a Date from a raw encoded value (Julian Day Number).
    ///
    /// This is an infallible version that does not validate the date range.
    /// Use `try_decode` for validation.
    pub const fn decode(encoded: u32) -> Self {
        // Bit-pattern reinterpret: Hyper wire format stores Julian day as u32;
        // we decode back into i32 by re-interpreting the bit pattern and then
        // undoing the wrapping-add from `encode`.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "intentional i32 bit-pattern reinterpret; inverse of Date::encode"
        )]
        let as_signed = encoded as i32;
        Date {
            days: as_signed.wrapping_sub(Self::JULIAN_DAY_EPOCH),
        }
    }

    /// Minimum supported Julian day (year 1, Jan 1)
    pub const MIN_JULIAN_DAY: i32 = 1721060;
    /// Maximum supported Julian day (year 9999, Dec 31)
    pub const MAX_JULIAN_DAY: i32 = 5373484;

    /// Creates a Date from a raw encoded value (Julian Day Number) with validation.
    ///
    /// Returns an error if the date is outside the supported range (years 1-9999)
    /// or if the calculation would overflow.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperdb_api_core::types::Date;
    ///
    /// // Valid date
    /// let date = Date::try_decode(2451545).unwrap(); // 2000-01-01
    ///
    /// // Invalid date (out of range)
    /// assert!(Date::try_decode(0).is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// - Returns an error if `encoded` (reinterpreted as `i32`) falls outside
    ///   the supported Julian day range [`Self::MIN_JULIAN_DAY`]..=[`Self::MAX_JULIAN_DAY`]
    ///   (years 1-9999).
    /// - Returns an error if the day offset from the epoch would overflow `i32`
    ///   or exceed `i32::MAX / 2`.
    pub fn try_decode(encoded: u32) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Bit-pattern reinterpret: wire format is u32; the validation below
        // catches the case where the decoded i32 is outside the supported range.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "intentional i32 bit-pattern reinterpret; range-checked immediately below"
        )]
        let julian_day = encoded as i32;

        // Validate Julian day is within supported range
        if !(Self::MIN_JULIAN_DAY..=Self::MAX_JULIAN_DAY).contains(&julian_day) {
            return Err(format!(
                "Julian day {} is out of supported range ({} to {}, years 1-9999)",
                julian_day,
                Self::MIN_JULIAN_DAY,
                Self::MAX_JULIAN_DAY
            )
            .into());
        }

        // Use checked arithmetic to prevent overflow in days calculation
        let days = julian_day
            .checked_sub(Self::JULIAN_DAY_EPOCH)
            .ok_or_else(|| {
                format!(
                    "Date calculation would overflow: Julian day {} - epoch {} exceeds i32 range",
                    julian_day,
                    Self::JULIAN_DAY_EPOCH
                )
            })?;

        // Additional bounds check to ensure days is within reasonable range
        // This provides defense-in-depth against edge cases
        if days.abs() > i32::MAX / 2 {
            return Err(format!(
                "Date calculation would overflow: days offset {days} exceeds safe range"
            )
            .into());
        }

        Ok(Date { days })
    }

    /// Returns the Julian Day Number for this date.
    ///
    /// Hyper stores dates as Julian Day Numbers (absolute day count).
    ///
    /// Uses wrapping arithmetic to avoid panic for out-of-range days values.
    /// Callers should validate dates are within supported range (years 1-9999)
    /// before relying on the Julian day value.
    pub const fn to_julian_day(&self) -> i32 {
        self.days.wrapping_add(Self::JULIAN_DAY_EPOCH)
    }

    // Internal: Convert YMD to days since 2000-01-01
    fn ymd_to_days(year: i32, month: u32, day: u32) -> i32 {
        // Algorithm based on PostgreSQL date calculations
        let mut y = year;
        // `month` / `day` are u32 in chrono's API but are always 1..=12 / 1..=31, so
        // they trivially fit in i32.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "month is 1..=12, always fits in i32"
        )]
        let mut m = month as i32;
        #[expect(
            clippy::cast_possible_wrap,
            reason = "day is 1..=31, always fits in i32"
        )]
        let d = day as i32;

        if m > 2 {
            m += 1;
            y += 4800;
        } else {
            m += 13;
            y += 4799;
        }

        let century = y / 100;
        let julian = y * 365 - 32167 + y / 4 - century + century / 4 + 7834 * m / 256 + d;

        // Convert from Julian to days since 2000-01-01
        // Julian day of 2000-01-01 is 2451545
        julian - 2451545
    }

    #[expect(
        clippy::many_single_char_names,
        reason = "date math algorithm: single-letter names match the Julian-day reference formula"
    )]
    // Internal: Convert days since 2000-01-01 to YMD
    fn days_to_ymd(days: i32) -> (i32, u32, u32) {
        // Convert to Julian day number
        let julian = days + 2451545;

        let a = julian + 32044;
        let b = (4 * a + 3) / 146097;
        let c = a - (b * 146097) / 4;

        let d = (4 * c + 3) / 1461;
        let e = c - (1461 * d) / 4;
        let m = (5 * e + 2) / 153;

        let day = e - (153 * m + 2) / 5 + 1;
        let month = m + 3 - 12 * (m / 10);
        let year = b * 100 + d - 4800 + m / 10;

        // `month` is always 1..=12 and `day` always 1..=31 by construction, so both
        // are non-negative and fit in u32.
        #[expect(
            clippy::cast_sign_loss,
            reason = "month/day are always positive by construction of days_to_ymd"
        )]
        let ymd = (year, month as u32, day as u32);
        ymd
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (year, month, day) = self.to_ymd();
        write!(f, "{year:04}-{month:02}-{day:02}")
    }
}

impl ToHyperBinary for Date {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        buf.put_i32_le(self.to_julian_day());
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        buf.put_i32_le(self.to_julian_day());
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 4
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        4
    }
}

impl FromHyperBinary for Date {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if buf.len() != 4 {
            return Err("invalid buffer size for Date".into());
        }
        let julian_day = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        // Bit-pattern reinterpret into the wire-format u32; `try_decode` validates range.
        #[expect(
            clippy::cast_sign_loss,
            reason = "intentional u32 bit-pattern reinterpret; try_decode validates range"
        )]
        let encoded = julian_day as u32;
        Date::try_decode(encoded)
    }
}

// =============================================================================
// Time - Microseconds since midnight
// =============================================================================

/// A time value (microseconds since midnight).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Time {
    /// Microseconds since midnight (0 to 86,399,999,999)
    microseconds: u64,
}

impl Time {
    /// Microseconds per second
    pub const MICROS_PER_SECOND: u64 = 1_000_000;
    /// Microseconds per minute
    pub const MICROS_PER_MINUTE: u64 = 60 * Self::MICROS_PER_SECOND;
    /// Microseconds per hour
    pub const MICROS_PER_HOUR: u64 = 60 * Self::MICROS_PER_MINUTE;

    /// Maximum valid microseconds value (24 hours - 1 microsecond).
    pub const MAX_MICROSECONDS: u64 = 24 * Self::MICROS_PER_HOUR - 1;

    /// Creates a Time from microseconds since midnight.
    ///
    /// No validation is performed. For a checked variant, use
    /// [`try_from_microseconds`](Self::try_from_microseconds).
    pub const fn from_microseconds(microseconds: u64) -> Self {
        Time { microseconds }
    }

    /// Creates a Time from microseconds since midnight with validation.
    ///
    /// # Errors
    ///
    /// Returns an error if `microseconds` > [`MAX_MICROSECONDS`](Self::MAX_MICROSECONDS)
    /// (i.e. >= 24 hours).
    pub fn try_from_microseconds(
        microseconds: u64,
    ) -> std::result::Result<Self, Box<dyn Error + Send + Sync>> {
        if microseconds > Self::MAX_MICROSECONDS {
            return Err(format!(
                "Time microseconds {microseconds} exceeds 24-hour limit ({})",
                Self::MAX_MICROSECONDS,
            )
            .into());
        }
        Ok(Time { microseconds })
    }

    /// Creates a Time from hour, minute, second, microsecond components.
    pub const fn new(hour: u32, minute: u32, second: u32, microsecond: u32) -> Self {
        let micros = (hour as u64) * Self::MICROS_PER_HOUR
            + (minute as u64) * Self::MICROS_PER_MINUTE
            + (second as u64) * Self::MICROS_PER_SECOND
            + microsecond as u64;
        Time {
            microseconds: micros,
        }
    }

    /// Returns microseconds since midnight.
    pub const fn microseconds(&self) -> u64 {
        self.microseconds
    }

    /// Returns hour, minute, second, microsecond components.
    pub const fn to_hms_micro(&self) -> (u32, u32, u32, u32) {
        // Only `hour` needs an explicit expect — clippy tracks the modulo bounds on
        // the others and can prove they fit in u32 automatically.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "micros / MICROS_PER_HOUR < 24 for valid Time values, fits in u32"
        )]
        let hour = (self.microseconds / Self::MICROS_PER_HOUR) as u32;
        let remaining = self.microseconds % Self::MICROS_PER_HOUR;
        let minute = (remaining / Self::MICROS_PER_MINUTE) as u32;
        let remaining = remaining % Self::MICROS_PER_MINUTE;
        let second = (remaining / Self::MICROS_PER_SECOND) as u32;
        let microsecond = (remaining % Self::MICROS_PER_SECOND) as u32;
        (hour, minute, second, microsecond)
    }

    /// Returns the raw encoded value.
    pub const fn encode(&self) -> u64 {
        self.microseconds
    }

    /// Creates a Time from a raw encoded value.
    pub const fn decode(encoded: u64) -> Self {
        Time {
            microseconds: encoded,
        }
    }

    /// Returns microseconds since midnight as i64 for insertion.
    ///
    /// This is the same as `microseconds()` but as signed for the Inserter API.
    pub const fn to_microseconds(&self) -> i64 {
        // `Time::microseconds` is bounded `0..86_400_000_000` (one day worth of µs)
        // which fits comfortably in i64.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "Time microseconds < 86_400_000_000 (1 day), fits in i64"
        )]
        let as_signed = self.microseconds as i64;
        as_signed
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (h, m, s, us) = self.to_hms_micro();
        if us == 0 {
            write!(f, "{h:02}:{m:02}:{s:02}")
        } else {
            write!(f, "{h:02}:{m:02}:{s:02}.{us:06}")
        }
    }
}

impl ToHyperBinary for Time {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        buf.put_u64_le(self.microseconds);
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        buf.put_u64_le(self.microseconds);
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 8
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        8
    }
}

impl FromHyperBinary for Time {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if buf.len() != 8 {
            return Err("invalid buffer size for Time".into());
        }
        let microseconds = u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        // Validate range — a malformed/hostile server could send an out-of-range
        // value (>= 24 hours) which would corrupt downstream `to_hms_micro` math.
        Time::try_from_microseconds(microseconds)
    }
}

// =============================================================================
// Timestamp - Microseconds since 2000-01-01 00:00:00
// =============================================================================

/// A timestamp value (microseconds since 2000-01-01 00:00:00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp {
    /// Microseconds since 2000-01-01 00:00:00 (can be negative)
    microseconds: i64,
}

impl Timestamp {
    /// Microseconds per day
    const MICROS_PER_DAY: i64 = 24 * 60 * 60 * 1_000_000;
    /// Offset in microseconds from 2000-01-01 to Julian Day 0.
    /// `JULIAN_DAY_EPOCH` is `2_451_545` (positive), so the i32 → i64 widening
    /// is lossless; use `from()` via `as` in a const context.
    const EPOCH_OFFSET_MICROS: i64 = Date::JULIAN_DAY_EPOCH as i64 * Self::MICROS_PER_DAY;

    /// Creates a Timestamp from microseconds since 2000-01-01.
    pub const fn from_microseconds(microseconds: i64) -> Self {
        Timestamp { microseconds }
    }

    /// Creates a Timestamp from date and time components.
    pub fn new(date: Date, time: Time) -> Self {
        // `date.days()` is i32 → i64 widening: always lossless.
        let date_micros = i64::from(date.days()) * Self::MICROS_PER_DAY;
        // `time.microseconds()` is u64 bounded by one day (< 86_400_000_000),
        // fits in i64.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "Time microseconds < 86_400_000_000 (1 day), fits in i64"
        )]
        let time_micros = time.microseconds() as i64;
        Timestamp {
            microseconds: date_micros + time_micros,
        }
    }

    /// Returns microseconds since 2000-01-01 (internal representation).
    pub const fn microseconds(&self) -> i64 {
        self.microseconds
    }

    /// Returns the date and time components.
    pub fn to_date_time(&self) -> (Date, Time) {
        // `div_euclid(MICROS_PER_DAY)` on i64 yields values bounded by
        // `i64::MAX / MICROS_PER_DAY ≈ 1.07e8`, well within i32 range for any
        // realistic Timestamp (calendar years fit easily).
        #[expect(
            clippy::cast_possible_truncation,
            reason = "i64 micros / MICROS_PER_DAY fits in i32 for any date in the supported calendar range (years 1-9999)"
        )]
        let days = self.microseconds.div_euclid(Self::MICROS_PER_DAY) as i32;
        // `rem_euclid(MICROS_PER_DAY)` is `0..MICROS_PER_DAY`, so clippy can prove
        // the cast is non-narrowing and doesn't flag it.
        let time_micros = self.microseconds.rem_euclid(Self::MICROS_PER_DAY) as u64;
        (Date::from_days(days), Time::from_microseconds(time_micros))
    }

    /// Returns the raw encoded value for HyperBinary (Julian-based microseconds).
    pub const fn encode(&self) -> u64 {
        // Bit-pattern reinterpret: wire format is u64. `wrapping_add` handles i64
        // edge cases; the `as u64` then reinterprets the bit pattern. Inverse of
        // `decode`.
        #[expect(
            clippy::cast_sign_loss,
            reason = "intentional u64 bit-pattern reinterpret; inverse of Timestamp::decode"
        )]
        let encoded = (self.microseconds.wrapping_add(Self::EPOCH_OFFSET_MICROS)) as u64;
        encoded
    }

    /// Creates a Timestamp from a raw encoded value (Julian-based microseconds).
    pub const fn decode(encoded: u64) -> Self {
        // Bit-pattern reinterpret: wire format is u64; we decode into i64 by
        // re-interpreting and then undoing the offset.
        #[expect(
            clippy::cast_possible_wrap,
            reason = "intentional i64 bit-pattern reinterpret; inverse of Timestamp::encode"
        )]
        let as_signed = encoded as i64;
        Timestamp {
            microseconds: as_signed.wrapping_sub(Self::EPOCH_OFFSET_MICROS),
        }
    }

    /// Returns microseconds since Julian Day 0 for binary insertion.
    ///
    /// Hyper stores timestamps as Julian-based microseconds.
    pub const fn to_microseconds(&self) -> i64 {
        self.microseconds + Self::EPOCH_OFFSET_MICROS
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (date, time) = self.to_date_time();
        write!(f, "{date} {time}")
    }
}

impl ToHyperBinary for Timestamp {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        buf.put_i64_le(self.to_microseconds());
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        buf.put_i64_le(self.to_microseconds());
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 8
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        8
    }
}

impl FromHyperBinary for Timestamp {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if buf.len() != 8 {
            return Err("invalid buffer size for Timestamp".into());
        }
        let julian_micros = u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        Ok(Timestamp::decode(julian_micros))
    }
}

// =============================================================================
// OffsetTimestamp - Timestamp with timezone offset
// =============================================================================

/// A timestamp with timezone offset.
///
/// Internally stored as UTC timestamp; the offset is for display purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OffsetTimestamp {
    /// UTC timestamp
    timestamp: Timestamp,
    /// Offset from UTC in minutes
    offset_minutes: i16,
}

impl OffsetTimestamp {
    /// Creates an OffsetTimestamp from a timestamp and UTC offset in minutes.
    pub const fn new(timestamp: Timestamp, offset_minutes: i16) -> Self {
        OffsetTimestamp {
            timestamp,
            offset_minutes,
        }
    }

    /// Creates an OffsetTimestamp from UTC timestamp and converts to specified offset.
    pub fn from_utc(utc_timestamp: Timestamp, offset_minutes: i16) -> Self {
        OffsetTimestamp {
            timestamp: utc_timestamp,
            offset_minutes,
        }
    }

    /// Returns the UTC timestamp.
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns the UTC offset in minutes.
    pub const fn offset_minutes(&self) -> i16 {
        self.offset_minutes
    }

    /// Returns the local timestamp (adjusted for offset).
    pub fn local_timestamp(&self) -> Timestamp {
        let offset_micros = i64::from(self.offset_minutes) * 60 * 1_000_000;
        Timestamp::from_microseconds(self.timestamp.microseconds() + offset_micros)
    }

    /// Returns the raw encoded value (UTC microseconds).
    pub const fn encode(&self) -> u64 {
        self.timestamp.encode()
    }

    /// Creates from a raw encoded value (assumes UTC, zero offset).
    pub const fn decode(encoded: u64) -> Self {
        OffsetTimestamp {
            timestamp: Timestamp::decode(encoded),
            offset_minutes: 0,
        }
    }

    /// Returns UTC microseconds (Julian-based) for binary insertion.
    ///
    /// Hyper stores timestamps as Julian-based microseconds.
    pub const fn to_microseconds_utc(&self) -> i64 {
        self.timestamp.to_microseconds()
    }
}

impl fmt::Display for OffsetTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local = self.local_timestamp();
        let (date, time) = local.to_date_time();
        let offset_hours = self.offset_minutes / 60;
        let offset_mins = (self.offset_minutes % 60).abs();
        let sign = if self.offset_minutes >= 0 { '+' } else { '-' };
        write!(
            f,
            "{} {}{}{:02}:{:02}",
            date,
            time,
            sign,
            offset_hours.abs(),
            offset_mins
        )
    }
}

impl ToHyperBinary for OffsetTimestamp {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Stored as UTC timestamp
        self.timestamp.to_hyper_binary(buf)
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.timestamp.to_hyper_binary_not_null(buf)
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        self.timestamp.hyper_binary_size()
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        self.timestamp.hyper_binary_size_not_null()
    }
}

impl FromHyperBinary for OffsetTimestamp {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let timestamp = Timestamp::from_hyper_binary(buf)?;
        Ok(OffsetTimestamp {
            timestamp,
            offset_minutes: 0, // Wire format doesn't include offset
        })
    }
}

// =============================================================================
// Interval - 128-bit packed interval
// =============================================================================

/// An interval value.
///
/// Hyper stores intervals as a 128-bit packed value with:
/// - microseconds (i64): time component
/// - days (i32): day component
/// - months (i32): month component
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interval {
    /// Microseconds component
    microseconds: i64,
    /// Days component
    days: i32,
    /// Months component
    months: i32,
}

impl Interval {
    /// Creates an Interval from components.
    pub const fn new(months: i32, days: i32, microseconds: i64) -> Self {
        Interval {
            microseconds,
            days,
            months,
        }
    }

    /// Creates an Interval from years.
    pub const fn from_years(years: i32) -> Self {
        Interval::new(years * 12, 0, 0)
    }

    /// Creates an Interval from months.
    pub const fn from_months(months: i32) -> Self {
        Interval::new(months, 0, 0)
    }

    /// Creates an Interval from days.
    pub const fn from_days(days: i32) -> Self {
        Interval::new(0, days, 0)
    }

    /// Creates an Interval from hours.
    pub const fn from_hours(hours: i64) -> Self {
        Interval::new(0, 0, hours * 3_600_000_000)
    }

    /// Creates an Interval from minutes.
    pub const fn from_minutes(minutes: i64) -> Self {
        Interval::new(0, 0, minutes * 60_000_000)
    }

    /// Creates an Interval from seconds.
    pub const fn from_seconds(seconds: i64) -> Self {
        Interval::new(0, 0, seconds * 1_000_000)
    }

    /// Creates an Interval from microseconds.
    pub const fn from_microseconds(microseconds: i64) -> Self {
        Interval::new(0, 0, microseconds)
    }

    /// Returns the months component.
    pub const fn months(&self) -> i32 {
        self.months
    }

    /// Returns the days component.
    pub const fn days(&self) -> i32 {
        self.days
    }

    /// Returns the microseconds component.
    pub const fn microseconds(&self) -> i64 {
        self.microseconds
    }

    /// Encodes the interval as a 128-bit value for HyperBinary.
    ///
    /// Layout: [microseconds: i64 LE][days: i32 LE][months: i32 LE]
    pub fn encode(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&self.microseconds.to_le_bytes());
        buf[8..12].copy_from_slice(&self.days.to_le_bytes());
        buf[12..16].copy_from_slice(&self.months.to_le_bytes());
        buf
    }

    /// Decodes an interval from a 128-bit value.
    pub fn decode(buf: &[u8; 16]) -> Self {
        let microseconds = i64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ]);
        let days = i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let months = i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Interval {
            microseconds,
            days,
            months,
        }
    }

    /// Returns the packed 128-bit representation for insertion.
    ///
    /// This is an alias for `encode()` for API consistency.
    pub fn to_packed(&self) -> [u8; 16] {
        self.encode()
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let years = self.months / 12;
        let months = self.months % 12;

        let mut parts = Vec::new();
        if years != 0 {
            parts.push(format!(
                "{} year{}",
                years,
                if years.abs() == 1 { "" } else { "s" }
            ));
        }
        if months != 0 {
            parts.push(format!(
                "{} mon{}",
                months,
                if months.abs() == 1 { "" } else { "s" }
            ));
        }
        if self.days != 0 {
            parts.push(format!(
                "{} day{}",
                self.days,
                if self.days.abs() == 1 { "" } else { "s" }
            ));
        }
        if self.microseconds != 0 || parts.is_empty() {
            let total_seconds = self.microseconds / 1_000_000;
            let micros = (self.microseconds % 1_000_000).abs();
            let hours = total_seconds / 3600;
            let minutes = (total_seconds % 3600) / 60;
            let seconds = total_seconds % 60;
            if micros == 0 {
                parts.push(format!(
                    "{:02}:{:02}:{:02}",
                    hours,
                    minutes.abs(),
                    seconds.abs()
                ));
            } else {
                parts.push(format!(
                    "{:02}:{:02}:{:02}.{:06}",
                    hours,
                    minutes.abs(),
                    seconds.abs(),
                    micros
                ));
            }
        }

        write!(f, "{}", parts.join(" "))
    }
}

impl ToHyperBinary for Interval {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        buf.extend_from_slice(&self.encode());
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        buf.extend_from_slice(&self.encode());
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 16
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        16
    }
}

impl FromHyperBinary for Interval {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if buf.len() != 16 {
            return Err("invalid buffer size for Interval".into());
        }
        let arr: [u8; 16] = buf.try_into().unwrap();
        Ok(Interval::decode(&arr))
    }
}

// =============================================================================
// Numeric - 128-bit arbitrary precision decimal
// =============================================================================

/// A numeric (decimal) value with up to 38 digits of precision.
///
/// Hyper stores `NUMERIC(precision, scale)` as a fixed-point integer with
/// implicit scale: the actual value is `unscaled_value × 10^(-scale)`.
/// The scale is determined by the column's type modifier.
///
/// # Wire Format
///
/// Hyper uses two wire representations depending on declared precision:
///
/// | Declared Precision | Wire Size | Rust Storage |
/// |---|---|---|
/// | ≤ 18 digits | 8 bytes (i64) | Widened to `i128` on read |
/// | 19–38 digits | 16 bytes (i128) | Stored directly |
///
/// This distinction is transparent to callers — `Numeric` always stores
/// an `i128` internally. The 18-digit threshold corresponds to
/// [`SMALL_NUMERIC_MAX_PRECISION`](Self::SMALL_NUMERIC_MAX_PRECISION).
///
/// The maximum precision of 38 digits ([`MAX_PRECISION`](Self::MAX_PRECISION))
/// matches the limit of a signed 128-bit integer (which can represent up
/// to ±1.7 × 10^38).
///
/// # No Arithmetic
///
/// `Numeric` does not implement arithmetic operators (`Add`, `Sub`, etc.)
/// because scale handling during arithmetic is context-dependent and best
/// done via SQL. Use [`to_f64`](Self::to_f64) for approximate arithmetic
/// in Rust, or perform calculations server-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Numeric {
    /// The unscaled value (multiply by 10^(-scale) for actual value)
    value: i128,
    /// The scale (number of digits after decimal point)
    scale: u8,
}

impl Numeric {
    /// Maximum precision (38 digits).
    ///
    /// This is the single source of truth for Hyper's NUMERIC precision limit.
    /// All validation elsewhere in the codebase should reference this constant.
    pub const MAX_PRECISION: u8 = 38;

    /// Maximum precision that fits in i64 storage (18 digits).
    ///
    /// NUMERIC values with precision ≤ 18 are stored as 64-bit integers in
    /// HyperBinary format. Precision > 18 uses 128-bit storage.
    pub const SMALL_NUMERIC_MAX_PRECISION: u32 = 18;

    /// Creates a Numeric from an unscaled value and scale.
    pub const fn new(value: i128, scale: u8) -> Self {
        Numeric { value, scale }
    }

    /// Creates a Numeric from an integer.
    pub const fn from_i128(value: i128) -> Self {
        Numeric { value, scale: 0 }
    }

    /// Creates a Numeric from an i64.
    pub const fn from_i64(value: i64) -> Self {
        Numeric {
            value: value as i128,
            scale: 0,
        }
    }

    /// Creates a Numeric from an i32.
    pub const fn from_i32(value: i32) -> Self {
        Numeric {
            value: value as i128,
            scale: 0,
        }
    }

    /// Returns the unscaled value.
    pub const fn unscaled_value(&self) -> i128 {
        self.value
    }

    /// Returns the scale.
    pub const fn scale(&self) -> u8 {
        self.scale
    }

    /// Converts to f64 (may lose precision).
    pub fn to_f64(&self) -> f64 {
        (self.value as f64) / 10f64.powi(i32::from(self.scale))
    }

    /// Maximum unscaled absolute value for a 38-digit Numeric.
    const MAX_UNSCALED: i128 = 10_i128.pow(38) - 1;

    /// Creates from f64 with specified scale.
    ///
    /// # Panics
    ///
    /// Panics if `value * 10^scale` is not finite or exceeds the 38-digit
    /// precision limit of Hyper's Numeric type.
    /// For a non-panicking variant, use [`try_from_f64`](Self::try_from_f64).
    pub fn from_f64(value: f64, scale: u8) -> Self {
        Self::try_from_f64(value, scale).expect("Numeric value exceeds 38-digit precision limit")
    }

    /// Creates from f64 with specified scale, returning an error if the value
    /// exceeds Hyper's 38-digit precision limit.
    ///
    /// # Errors
    ///
    /// Returns an error if the scaled value exceeds ±(10^38 - 1) or if the
    /// intermediate result is not finite.
    pub fn try_from_f64(
        value: f64,
        scale: u8,
    ) -> std::result::Result<Self, Box<dyn Error + Send + Sync>> {
        let multiplier = 10f64.powi(i32::from(scale));
        let scaled = value * multiplier;
        if !scaled.is_finite() {
            return Err(format!(
                "Numeric::from_f64({value}, {scale}): intermediate value is not finite"
            )
            .into());
        }
        let rounded = scaled.round();
        // Reject pre-cast — `f64 as i128` is saturating since Rust 1.45, which
        // would silently clamp huge values to ±i128::MAX and produce a misleading
        // error message ("unscaled: 170141..." for an input of 1e50).
        // i128::MAX ≈ 1.7e38; 38-digit max is 1e38 - 1, both well below f64's
        // exactly-representable range.
        #[expect(
            clippy::cast_precision_loss,
            reason = "i128 bounds as f64 — only used for range check, precision loss is irrelevant"
        )]
        let i128_max_f64 = i128::MAX as f64;
        #[expect(
            clippy::cast_precision_loss,
            reason = "i128 bounds as f64 — only used for range check, precision loss is irrelevant"
        )]
        let i128_min_f64 = i128::MIN as f64;
        if rounded > i128_max_f64 || rounded < i128_min_f64 {
            return Err(format!(
                "Numeric value {value} with scale {scale} exceeds i128 range after scaling \
                 (scaled: {rounded}, max ±{})",
                Self::MAX_UNSCALED,
            )
            .into());
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "validated against i128 range immediately above"
        )]
        let unscaled = rounded as i128;
        if unscaled.abs() > Self::MAX_UNSCALED {
            return Err(format!(
                "Numeric value {value} with scale {scale} exceeds 38-digit precision limit \
                 (unscaled: {unscaled}, max: ±{})",
                Self::MAX_UNSCALED,
            )
            .into());
        }
        Ok(Numeric {
            value: unscaled,
            scale,
        })
    }

    /// Encodes the numeric as a 128-bit little-endian value, suitable for
    /// the HyperBinary `BigNumeric` wire format (precision > 18).
    pub fn encode(&self) -> [u8; 16] {
        self.value.to_le_bytes()
    }

    /// Encodes the numeric as a 64-bit little-endian value, suitable for
    /// the HyperBinary `Numeric` wire format (precision ≤ 18).
    ///
    /// # Errors
    ///
    /// Returns an `Err` if `self.value` is outside the `i64` range.
    /// This happens exactly when the unscaled value exceeds what a 19+
    /// digit precision can hold, i.e. the value belongs in the
    /// `BigNumeric` 16-byte wire form instead. The error names the
    /// specific out-of-range value so callers have something
    /// actionable in logs.
    ///
    /// Returns `Ok` for every value that fits; callers who can
    /// statically prove their values are in range (e.g. building from
    /// a `NUMERIC(p, s)` schema with `p ≤ 18`) can `.expect()` the
    /// result without losing any safety.
    ///
    /// For values that are always representable in 128 bits — i.e.
    /// anything Hyper can hold in any `NUMERIC(p, s)` with `p ≤ 38` —
    /// use [`encode`](Self::encode) instead, which is infallible.
    pub fn encode_int64(&self) -> Result<[u8; 8], Box<dyn Error + Send + Sync>> {
        // `as i64` is a truncating cast that silently wraps on
        // overflow — catastrophic for wire formats because the
        // server would read a valid-looking but semantically wrong
        // value. `i64::try_from` is the stdlib-sanctioned checked
        // conversion; use it to surface the overflow as an error
        // instead of silent data corruption.
        let value_i64 = i64::try_from(self.value).map_err(|_| -> Box<dyn Error + Send + Sync> {
            format!(
                "Numeric::encode_int64 called with value {} outside i64 range; \
                 precision must be ≤ {} for the 64-bit wire form (use Numeric::encode \
                 for the 16-byte form instead)",
                self.value,
                Self::SMALL_NUMERIC_MAX_PRECISION,
            )
            .into()
        })?;
        Ok(value_i64.to_le_bytes())
    }

    /// Decodes a numeric from the 128-bit HyperBinary `BigNumeric` wire
    /// form (precision > 18). See [`decode_int64`](Self::decode_int64) for
    /// the 8-byte `Numeric` form used when precision ≤ 18.
    ///
    /// The `scale` parameter is not carried in the wire bytes — it must
    /// come from the column's `SqlType::Numeric { precision, scale }`
    /// descriptor. See `hyperdb_api::result::Rowset::schema()` for how this
    /// flows end-to-end from a `RowDescription` message.
    pub fn decode(buf: &[u8; 16], scale: u8) -> Self {
        let value = i128::from_le_bytes(*buf);
        Numeric { value, scale }
    }

    /// Decodes a numeric from the 64-bit HyperBinary `Numeric` wire form
    /// (used by Hyper when precision ≤ 18 — see
    /// `Type::maxPrecisionNumeric = 18` in the Hyper DB source at
    /// `hyper/rts/type/Type.hpp`). The 64-bit form is what
    /// `AVG(SmallInt | Integer)` returns (as `Numeric(11, 6)` or
    /// `Numeric(16, 6)` respectively — see `AggregationLogic::
    /// dividingAggregateType` in
    /// `hyper/cts/algebra/operator/AggregationLogic.cpp`).
    ///
    /// Like [`decode`](Self::decode), `scale` is not carried in the
    /// bytes and must come from the column's `SqlType::Numeric`
    /// descriptor.
    pub fn decode_int64(buf: &[u8; 8], scale: u8) -> Self {
        let value = i128::from(i64::from_le_bytes(*buf));
        Numeric { value, scale }
    }

    /// Returns the packed 128-bit representation for insertion.
    ///
    /// This is an alias for `encode()` for API consistency.
    pub fn to_packed(&self) -> [u8; 16] {
        self.encode()
    }
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            write!(f, "{}", self.value)
        } else {
            // Compute the sign explicitly and format the magnitude. Deriving
            // the sign from `int_part` alone loses it whenever `|value| < 1`
            // (the integer part is `0`, which prints without a sign), so
            // values like -0.5 would render as "0.5000". `unsigned_abs` also
            // avoids the `i128::MIN` overflow that `.abs()` would hit.
            let divisor = 10u128.pow(u32::from(self.scale));
            let sign = if self.value < 0 { "-" } else { "" };
            let abs = self.value.unsigned_abs();
            let int_part = abs / divisor;
            let frac_part = abs % divisor;
            write!(
                f,
                "{sign}{int_part}.{frac_part:0width$}",
                width = self.scale as usize
            )
        }
    }
}

impl ToHyperBinary for Numeric {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        buf.extend_from_slice(&self.encode());
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        buf.extend_from_slice(&self.encode());
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 16
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        16
    }
}

impl Numeric {
    /// Creates a Numeric from binary data with explicit scale.
    ///
    /// This is the preferred method for decoding Numeric values from binary data,
    /// as the scale must be obtained from the column's type metadata.
    ///
    /// Dispatches on buffer size to handle both of Hyper's `NUMERIC` wire
    /// forms:
    ///
    /// - **8 bytes** (i64): used when the column's precision is ≤ 18
    ///   (Hyper's `Type::Numeric`; see `maxPrecisionNumeric = 18` in
    ///   `hyper/rts/type/Type.hpp`). This is the form returned by
    ///   aggregates like `AVG(INTEGER)` which Hyper types as
    ///   `Numeric(16, 6)` — see
    ///   `AggregationLogic::dividingAggregateType` in
    ///   `hyper/cts/algebra/operator/AggregationLogic.cpp`.
    /// - **16 bytes** (i128): used when the column's precision is
    ///   > 18 (Hyper's `Type::BigNumeric`, up to 38 digits).
    ///
    /// Any other buffer size is rejected with an error — callers that
    /// reach this function with, say, a 4-byte truncated read should
    /// surface the bug rather than silently decoding garbage.
    ///
    /// # Arguments
    ///
    /// * `buf` - An 8-byte or 16-byte buffer containing the unscaled
    ///   value in little-endian
    /// * `scale` - The number of digits after the decimal point, from
    ///   type metadata (`SqlType::Numeric { scale, .. }`)
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperdb_api_core::types::Numeric;
    ///
    /// // 16-byte form: NUMERIC(25, 2) value = 100
    /// let bytes16 = [100u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    /// let numeric = Numeric::from_binary_with_scale(&bytes16, 2).unwrap();
    /// assert_eq!(numeric.to_f64(), 1.00);
    ///
    /// // 8-byte form: NUMERIC(16, 6) = 2.0 (as returned by AVG(INTEGER))
    /// let bytes8: [u8; 8] = 2_000_000i64.to_le_bytes();
    /// let numeric = Numeric::from_binary_with_scale(&bytes8, 6).unwrap();
    /// assert_eq!(numeric.to_f64(), 2.0);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if `buf.len()` is anything other than 8 or 16 bytes.
    /// Hyper encodes `NUMERIC(precision, scale)` values in exactly one of
    /// those two widths, so any other size indicates a truncated read or
    /// protocol violation upstream.
    ///
    /// # Panics
    ///
    /// Does not panic in practice: the `buf.len() == 8` and `buf.len() == 16`
    /// branches each use `try_into().expect(...)` whose invariant is proven
    /// by the enclosing match arm.
    #[inline]
    pub fn from_binary_with_scale(
        buf: &[u8],
        scale: u8,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match buf.len() {
            8 => {
                // The `.expect` is documentation, not a runtime concern:
                // the match arm `8 =>` proves `buf.len() == 8`, so the
                // `&[u8]` → `[u8; 8]` conversion cannot fail. Using
                // `expect` over `unwrap` so a future refactor that
                // widens this arm (e.g. to accept a range) trips a
                // clear message instead of a bare "unwrap failed".
                let arr: [u8; 8] = buf
                    .try_into()
                    .expect("buf.len() == 8 is guaranteed by the match arm");
                Ok(Numeric::decode_int64(&arr, scale))
            }
            16 => {
                let arr: [u8; 16] = buf
                    .try_into()
                    .expect("buf.len() == 16 is guaranteed by the match arm");
                Ok(Numeric::decode(&arr, scale))
            }
            other => Err(format!(
                "invalid buffer size for Numeric (expected 8 or 16 bytes, got {other})"
            )
            .into()),
        }
    }
}

// NOTE: FromHyperBinary is intentionally NOT implemented for Numeric.
//
// The binary representation of a Numeric only contains the unscaled value —
// neither the scale nor the width (8 vs 16 bytes, depending on precision)
// is carried in the wire bytes. Both must come from the column's type
// metadata (`SqlType::Numeric { precision, scale }`). A generic
// `FromHyperBinary` implementation would have to default them (e.g., scale
// to 0 and width to 16), which would silently corrupt decimal values and
// fail to decode the 8-byte form used for precision ≤ 18 — including
// aggregate results like `AVG(INTEGER)` which Hyper types as
// `Numeric(16, 6)`.
//
// Use `Numeric::from_binary_with_scale(buf, scale)` instead. It accepts
// both 8-byte and 16-byte wire forms and takes the scale as an explicit
// argument.

// =============================================================================
// Geography - Spatial data with optional WKT/WKB support
// =============================================================================

/// A geography (spatial) value.
///
/// This type wraps raw geography bytes and provides type safety when working
/// with spatial data. With the `geography` feature enabled, it also supports
/// WKT/WKB parsing and conversion to/from `geo_types::Geometry`.
///
/// # Binary Format Notes
///
/// **Important**: This type can hold data in two different binary formats:
///
/// 1. **Hyper's Legacy Format**: Data read from Hyper query results is stored in
///    Hyper's proprietary legacy serialization format. This format is **not** WKB-compatible.
///
/// 2. **WKB Format**: When created via `from_wkt()` or `from_wkb()` (with the
///    `geography` feature), data is stored in WKB (Well-Known Binary) format.
///
/// Methods like `to_geometry()` and `to_wkt()` (available with the `geography`
/// feature) expect WKB format and will **fail** when called on data in Hyper's
/// legacy format.
///
/// # Basic Usage (Always Available)
///
/// ```no_run
/// use hyperdb_api_core::types::Geography;
///
/// // Create from raw bytes (e.g., from query results)
/// let geo = Geography::from_bytes(vec![0x01, 0x02, 0x03]);
///
/// // Get raw bytes for insertion
/// let bytes: &[u8] = geo.as_bytes();
/// ```
///
/// # WKT/WKB Parsing (Requires `geography` Feature)
///
/// ```ignore
/// use hyperdb_api_core::types::Geography;
///
/// // Create from WKT string (requires "geography" feature)
/// let geo = Geography::from_wkt("POINT(-122.4194 37.7749)")?;
///
/// // Convert to geo-types for processing
/// let geometry = geo.to_geometry()?;
///
/// // Export as WKT
/// let wkt_string = geo.to_wkt()?;
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Geography {
    /// Raw bytes in either Hyper's legacy serialization format or WKB format.
    data: Vec<u8>,
    /// The format of the binary data.
    format: GeographyBinaryFormat,
}

/// Indicates the binary format of a Geography value.
///
/// This enum explicitly tracks whether the binary data is in Hyper's proprietary
/// legacy format or in standard WKB (Well-Known Binary) format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GeographyBinaryFormat {
    /// Hyper's proprietary legacy serialization format.
    ///
    /// Data read from Hyper query results is in this format.
    /// **Important**: Methods like `to_geometry()` and `to_wkt()` will fail
    /// on data in this format.
    #[default]
    HyperLegacy,
    /// Standard Well-Known Binary (WKB) format.
    ///
    /// Data created via `from_wkt()` or `from_wkb()` is in this format.
    /// Methods like `to_geometry()` and `to_wkt()` work with this format.
    Wkb,
}

impl Geography {
    /// Creates a `Geography` from raw binary data in Hyper's legacy format.
    ///
    /// Use this when reading geography data from Hyper query results.
    /// The data will be in Hyper's internal legacy serialization format.
    ///
    /// # Format Compatibility
    ///
    /// Data read from Hyper is in Hyper's proprietary legacy format, **not** WKB.
    /// Methods like `to_geometry()` and `to_wkt()` (available with the `geography`
    /// feature) expect WKB format and will **fail** when called on data created
    /// via this method. Use `format()` to check the format before calling these methods.
    pub fn from_bytes(data: impl Into<Vec<u8>>) -> Self {
        Self {
            data: data.into(),
            format: GeographyBinaryFormat::HyperLegacy,
        }
    }

    /// Creates a `Geography` from WKB (Well-Known Binary) data.
    ///
    /// Use this when you have WKB data from an external source.
    /// Methods like `to_geometry()` and `to_wkt()` will work on this data.
    pub fn from_wkb_bytes(data: impl Into<Vec<u8>>) -> Self {
        Self {
            data: data.into(),
            format: GeographyBinaryFormat::Wkb,
        }
    }

    /// Returns the binary format of this geography value.
    ///
    /// Use this to check whether WKB-dependent methods like `to_geometry()`
    /// and `to_wkt()` will work on this value.
    ///
    /// Note: When the `geography` feature is enabled, there's also a `format()`
    /// method that performs runtime detection and returns the actual data.
    pub fn binary_format(&self) -> GeographyBinaryFormat {
        self.format
    }

    /// Returns true if this geography is in WKB format.
    ///
    /// WKB-dependent methods like `to_geometry()` and `to_wkt()` will only
    /// work if this returns `true`.
    pub fn is_wkb(&self) -> bool {
        self.format == GeographyBinaryFormat::Wkb
    }

    /// Returns true if this geography is in Hyper's legacy format.
    ///
    /// Data in this format cannot be converted to geometry or WKT.
    pub fn is_hyper_legacy(&self) -> bool {
        self.format == GeographyBinaryFormat::HyperLegacy
    }

    /// Returns the raw bytes of this geography value.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Consumes self and returns the underlying bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Returns the length of the binary data in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns true if the geography data is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl std::fmt::Debug for Geography {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Ok(wkt) = self.to_wkt() {
            return f
                .debug_struct("Geography")
                .field("wkt", &wkt)
                .field("bytes", &self.data.len())
                .finish_non_exhaustive();
        }

        f.debug_struct("Geography")
            .field("bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for Geography {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Ok(wkt) = self.to_wkt() {
            return write!(f, "{wkt}");
        }

        write!(f, "Geography({} bytes)", self.data.len())
    }
}

impl AsRef<[u8]> for Geography {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl From<Vec<u8>> for Geography {
    /// Creates a Geography from bytes, assuming Hyper's legacy format.
    ///
    /// Use `Geography::from_wkb_bytes()` if the data is in WKB format.
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<&[u8]> for Geography {
    /// Creates a Geography from bytes, assuming Hyper's legacy format.
    ///
    /// Use `Geography::from_wkb_bytes()` if the data is in WKB format.
    fn from(bytes: &[u8]) -> Self {
        Self::from_bytes(bytes.to_vec())
    }
}

impl From<Geography> for Vec<u8> {
    fn from(geo: Geography) -> Self {
        geo.into_bytes()
    }
}

impl ToHyperBinary for Geography {
    #[inline]
    fn to_hyper_binary(&self, buf: &mut BytesMut) -> Result<(), Box<dyn Error + Send + Sync>> {
        write_not_null_indicator(buf);
        let len = u32::try_from(self.data.len())
            .map_err(|_| "geography length exceeds HyperBinary 4-byte length prefix (u32::MAX)")?;
        buf.put_u32_le(len);
        buf.put_slice(&self.data);
        Ok(())
    }

    #[inline]
    fn to_hyper_binary_not_null(
        &self,
        buf: &mut BytesMut,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let len = u32::try_from(self.data.len())
            .map_err(|_| "geography length exceeds HyperBinary 4-byte length prefix (u32::MAX)")?;
        buf.put_u32_le(len);
        buf.put_slice(&self.data);
        Ok(())
    }

    #[inline]
    fn hyper_binary_size(&self) -> usize {
        NULL_INDICATOR_SIZE + 4 + self.data.len()
    }

    #[inline]
    fn hyper_binary_size_not_null(&self) -> usize {
        4 + self.data.len()
    }
}

impl FromHyperBinary for Geography {
    #[inline]
    fn from_hyper_binary(buf: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Ok(Geography {
            data: buf.to_vec(),
            format: GeographyBinaryFormat::HyperLegacy,
        })
    }
}

// =============================================================================
// Geography - WKT/WKB support (requires `geography` feature)
// =============================================================================

mod geo_impl {
    use super::{Error, Geography, GeographyBinaryFormat};
    use geo_types::{Coord, Geometry, LineString};
    use geozero::{CoordDimensions, ToWkb};
    use wkt::TryFromWkt;

    /// Error type for geography operations.
    #[derive(Debug)]
    pub struct GeoError(pub String);

    /// Geography binary encoding format.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GeographyFormat {
        /// Hyper legacy internal serialization (non-WKB).
        Legacy(Vec<u8>),
        /// Standard WKB encoding.
        Wkb(Vec<u8>),
    }

    impl std::fmt::Display for GeoError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl std::error::Error for GeoError {}

    impl Geography {
        /// Creates a `Geography` from a WKT (Well-Known Text) string.
        ///
        /// This parses the WKT string and converts it to WKB (Well-Known Binary)
        /// format for storage. Coordinates are validated to be within valid
        /// geographic ranges (longitude: [-180, 180], latitude: [-90, 90]).
        ///
        /// # Supported WKT Types
        ///
        /// - `POINT(x y)` - A single point
        /// - `LINESTRING(x1 y1, x2 y2, ...)` - A line
        /// - `POLYGON((x1 y1, x2 y2, ..., x1 y1))` - A polygon
        /// - `MULTIPOINT((x1 y1), (x2 y2), ...)` - Multiple points
        /// - `MULTILINESTRING((...), (...))` - Multiple lines
        /// - `MULTIPOLYGON(((...)), ((...)))` - Multiple polygons
        /// - `GEOMETRYCOLLECTION(...)` - Collection of geometries
        ///
        /// # Example
        ///
        /// ```ignore
        /// use hyperdb_api_core::types::Geography;
        ///
        /// // Requires "geography" feature
        /// let point = Geography::from_wkt("POINT(-122.4194 37.7749)")?;
        /// let line = Geography::from_wkt("LINESTRING(0 0, 1 1, 2 2)")?;
        /// let polygon = Geography::from_wkt("POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))")?;
        /// ```
        ///
        /// # Errors
        ///
        /// Returns an error if the WKT string is invalid or contains out-of-range coordinates.
        pub fn from_wkt(wkt_str: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
            // Parse WKT string to geo_types::Geometry
            let geometry: Geometry<f64> = Geometry::try_from_wkt_str(wkt_str)
                .map_err(|e| GeoError(format!("Invalid WKT '{wkt_str}': {e:?}")))?;

            // Validate coordinate ranges
            Self::validate_geometry_bounds(&geometry)?;

            // Convert to WKB
            let wkb_data = geometry
                .to_wkb(CoordDimensions::xy())
                .map_err(|e| GeoError(format!("Failed to convert geometry to WKB: {e}")))?;

            Ok(Self {
                data: wkb_data,
                format: GeographyBinaryFormat::Wkb,
            })
        }

        /// Creates a `Geography` from WKB (Well-Known Binary) data.
        ///
        /// This accepts standard WKB format and validates it eagerly.
        ///
        /// # Errors
        ///
        /// Returns an error if the WKB data is invalid.
        pub fn from_wkb(wkb_bytes: &[u8]) -> Result<Self, Box<dyn Error + Send + Sync>> {
            Self::validate_wkb_bytes(wkb_bytes)?;
            Ok(Self {
                data: wkb_bytes.to_vec(),
                format: GeographyBinaryFormat::Wkb,
            })
        }

        /// Validates that the stored data is valid WKB.
        ///
        /// Calling this on geography values sourced from Hyper query results
        /// (which are in Hyper's legacy format, not WKB) will return an error.
        ///
        /// # Errors
        ///
        /// - Returns an error if the payload is fewer than 9 bytes (the
        ///   minimum WKB envelope size).
        /// - Returns an error if the leading endianness marker is not
        ///   `0x00` (big-endian) or `0x01` (little-endian).
        /// - Returns an error if the bytes cannot be parsed as a valid
        ///   WKB geometry via `geozero`.
        pub fn validate_wkb_format(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
            Self::validate_wkb_bytes(&self.data)
        }

        /// Detects the underlying binary format (legacy vs WKB).
        ///
        /// This helps avoid calling WKB-only methods on legacy data by making the
        /// format explicit. Returns an error for empty payloads.
        ///
        /// # Errors
        ///
        /// Returns an error if the geography data is empty. A non-empty payload
        /// that fails WKB validation is reported as `GeographyFormat::Legacy`
        /// rather than an error.
        pub fn format(&self) -> Result<GeographyFormat, Box<dyn Error + Send + Sync>> {
            if self.data.is_empty() {
                return Err(Box::new(GeoError("Geography data is empty".into())));
            }

            if Self::validate_wkb_bytes(&self.data).is_ok() {
                return Ok(GeographyFormat::Wkb(self.data.clone()));
            }

            Ok(GeographyFormat::Legacy(self.data.clone()))
        }

        fn validate_wkb_bytes(bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            if bytes.len() < 9 {
                return Err(Box::new(GeoError(format!(
                    "Invalid WKB: too short ({} bytes)",
                    bytes.len()
                ))));
            }

            // WKB endianness marker must be 0x00 (big endian) or 0x01 (little endian)
            let endian = bytes[0];
            if endian != 0x00 && endian != 0x01 {
                return Err(Box::new(GeoError(format!(
                    "Invalid WKB: bad endianness marker {:#04x} ({} bytes)",
                    endian,
                    bytes.len()
                ))));
            }

            use geozero::wkb::Wkb;
            use geozero::ToGeo;

            let wkb = Wkb(bytes.to_vec());
            wkb.to_geo().map(|_: Geometry<f64>| ()).map_err(|e| {
                Box::new(GeoError(format!(
                    "Invalid WKB ({} bytes): {}",
                    bytes.len(),
                    e
                ))) as Box<dyn Error + Send + Sync>
            })
        }

        /// Creates a `Geography` from a `geo_types::Geometry`.
        ///
        /// Coordinates are validated to be within valid geographic ranges.
        ///
        /// # Example
        ///
        /// ```ignore
        /// use hyperdb_api_core::types::Geography;
        /// use geo_types::Point;
        ///
        /// // Requires "geography" feature
        /// let point = Point::new(-122.4194, 37.7749);
        /// let geo = Geography::from_geometry(&point.into())?;
        /// ```
        ///
        /// # Errors
        ///
        /// - Returns an error if any coordinate's longitude falls outside
        ///   `[-180, 180]` or latitude outside `[-90, 90]`.
        /// - Returns an error if a polygon ring is not closed within
        ///   [`Self::DEFAULT_RING_CLOSURE_EPSILON`].
        /// - Returns an error if `geozero` fails to serialize the geometry
        ///   to WKB.
        pub fn from_geometry(
            geometry: &Geometry<f64>,
        ) -> Result<Self, Box<dyn Error + Send + Sync>> {
            Self::validate_geometry_bounds(geometry)?;

            let wkb_data = geometry
                .to_wkb(CoordDimensions::xy())
                .map_err(|e| GeoError(format!("Failed to convert geometry to WKB: {e}")))?;

            Ok(Self {
                data: wkb_data,
                format: GeographyBinaryFormat::Wkb,
            })
        }

        /// Converts this geography to a `geo_types::Geometry`.
        ///
        /// # Format Requirements
        ///
        /// **This method requires WKB format**. It will **fail** if called on data
        /// created via `from_bytes()` with data read from Hyper query results.
        /// Use `is_wkb()` or `binary_format()` to check the format before calling.
        ///
        /// # Errors
        ///
        /// - Returns an error if this value is in Hyper's legacy format
        ///   (`GeographyBinaryFormat::HyperLegacy`). Construct via
        ///   [`Self::from_wkt`] or [`Self::from_wkb`] to use this method.
        /// - Returns an error if the stored bytes cannot be parsed as WKB
        ///   by `geozero`.
        pub fn to_geometry(&self) -> Result<Geometry<f64>, Box<dyn Error + Send + Sync>> {
            use geozero::wkb::Wkb;
            use geozero::ToGeo;

            if self.format == GeographyBinaryFormat::HyperLegacy {
                return Err(Box::new(GeoError(
                    "Cannot convert geography to geometry: data is in Hyper's legacy format, not WKB. \
                     Use Geography::from_wkt() or Geography::from_wkb() to create WKB-format geography.".into()
                )));
            }

            let wkb = Wkb(self.data.clone());
            wkb.to_geo().map_err(|e| {
                Box::new(GeoError(format!(
                    "Failed to parse geography as geometry ({} bytes, expected WKB format): {}",
                    self.data.len(),
                    e
                ))) as Box<dyn Error + Send + Sync>
            })
        }

        /// Exports this geography as a WKT (Well-Known Text) string.
        ///
        /// # Format Requirements
        ///
        /// **This method requires WKB format**. It will **fail** if called on data
        /// created via `from_bytes()` with data read from Hyper query results.
        /// Use `is_wkb()` or `binary_format()` to check the format before calling.
        ///
        /// # Errors
        ///
        /// - Returns an error if this value is in Hyper's legacy format
        ///   (`GeographyBinaryFormat::HyperLegacy`). Construct via
        ///   [`Self::from_wkt`] or [`Self::from_wkb`] to use this method.
        /// - Returns an error if the stored bytes cannot be serialized to
        ///   WKT by `geozero` (malformed WKB).
        pub fn to_wkt(&self) -> Result<String, Box<dyn Error + Send + Sync>> {
            use geozero::wkb::Wkb;
            use geozero::ToWkt;

            if self.format == GeographyBinaryFormat::HyperLegacy {
                return Err(Box::new(GeoError(
                    "Cannot convert geography to WKT: data is in Hyper's legacy format, not WKB. \
                     Use Geography::from_wkt() or Geography::from_wkb() to create WKB-format geography.".into()
                )));
            }

            let wkb = Wkb(self.data.clone());
            wkb.to_wkt().map_err(|e| {
                Box::new(GeoError(format!(
                    "Failed to convert geography to WKT ({} bytes, expected WKB format): {}",
                    self.data.len(),
                    e
                ))) as Box<dyn Error + Send + Sync>
            })
        }

        /// Returns the WKB data (clone of internal bytes).
        pub fn to_wkb(&self) -> Vec<u8> {
            self.data.clone()
        }

        /// Validates geometry coordinate ranges.
        /// Default tolerance for ring closure validation.
        /// This is more lenient than typical machine epsilon to accommodate
        /// geographic data that may have slightly different precision.
        pub const DEFAULT_RING_CLOSURE_EPSILON: f64 = 1e-6;

        fn validate_geometry_bounds(
            geometry: &Geometry<f64>,
        ) -> Result<(), Box<dyn Error + Send + Sync>> {
            Self::validate_geometry_bounds_with_tolerance(
                geometry,
                Self::DEFAULT_RING_CLOSURE_EPSILON,
            )
        }

        /// Validates geometry bounds with a configurable ring closure tolerance.
        ///
        /// Use this method when you need a different tolerance for ring closure
        /// validation (e.g., for data with lower precision).
        ///
        /// # Arguments
        ///
        /// * `geometry` - The geometry to validate
        /// * `ring_closure_epsilon` - The tolerance for ring closure validation.
        ///   Use a larger value for data with lower precision.
        ///
        /// # Errors
        ///
        /// - Returns an error if any coordinate longitude is outside
        ///   `[-180, 180]` or latitude outside `[-90, 90]`.
        /// - Returns an error if a polygon ring's first and last vertices
        ///   differ by more than `ring_closure_epsilon` on either axis.
        pub fn validate_geometry_bounds_with_tolerance(
            geometry: &Geometry<f64>,
            ring_closure_epsilon: f64,
        ) -> Result<(), Box<dyn Error + Send + Sync>> {
            const MIN_LONGITUDE: f64 = -180.0;
            const MAX_LONGITUDE: f64 = 180.0;
            const MIN_LATITUDE: f64 = -90.0;
            const MAX_LATITUDE: f64 = 90.0;

            let validate_coord = |coord: &Coord<f64>| -> Result<(), Box<dyn Error + Send + Sync>> {
                if coord.x < MIN_LONGITUDE || coord.x > MAX_LONGITUDE {
                    return Err(Box::new(GeoError(format!(
                        "Longitude {} is out of valid range [-180, 180]",
                        coord.x
                    ))));
                }
                if coord.y < MIN_LATITUDE || coord.y > MAX_LATITUDE {
                    return Err(Box::new(GeoError(format!(
                        "Latitude {} is out of valid range [-90, 90]",
                        coord.y
                    ))));
                }
                Ok(())
            };

            let validate_linestring =
                |ls: &LineString<f64>| -> Result<(), Box<dyn Error + Send + Sync>> {
                    if ls.0.is_empty() {
                        return Err(Box::new(GeoError("LineString cannot be empty".into())));
                    }
                    for coord in &ls.0 {
                        validate_coord(coord)?;
                    }
                    Ok(())
                };

            let rings_close = |first: &Coord<f64>, last: &Coord<f64>, tolerance: f64| -> bool {
                let dx = (first.x - last.x).abs();
                let dy = (first.y - last.y).abs();
                dx <= tolerance && dy <= tolerance
            };

            match geometry {
                Geometry::Point(p) => validate_coord(&p.0)?,
                Geometry::LineString(ls) => validate_linestring(ls)?,
                Geometry::Line(line) => {
                    validate_coord(&line.start)?;
                    validate_coord(&line.end)?;
                }
                Geometry::Polygon(poly) => {
                    let exterior = poly.exterior();
                    validate_linestring(exterior)?;
                    if exterior.0.len() < 4 {
                        return Err(Box::new(GeoError(
                            "Polygon exterior ring must have at least 4 points".into(),
                        )));
                    }
                    let first = &exterior.0[0];
                    let last = &exterior.0[exterior.0.len() - 1];
                    if !rings_close(first, last, ring_closure_epsilon) {
                        return Err(Box::new(GeoError(
                            "Polygon exterior ring must be closed".into(),
                        )));
                    }
                    for interior in poly.interiors() {
                        validate_linestring(interior)?;
                        if interior.0.len() >= 4 {
                            let first = &interior.0[0];
                            let last = &interior.0[interior.0.len() - 1];
                            if !rings_close(first, last, ring_closure_epsilon) {
                                return Err(Box::new(GeoError(
                                    "Polygon interior ring must be closed".into(),
                                )));
                            }
                        }
                    }
                }
                Geometry::MultiPoint(mp) => {
                    for point in mp {
                        validate_coord(&point.0)?;
                    }
                }
                Geometry::MultiLineString(mls) => {
                    for ls in mls {
                        validate_linestring(ls)?;
                    }
                }
                Geometry::MultiPolygon(mp) => {
                    for poly in mp {
                        Self::validate_geometry_bounds(&Geometry::Polygon(poly.clone()))?;
                    }
                }
                Geometry::GeometryCollection(gc) => {
                    for geom in gc {
                        Self::validate_geometry_bounds(geom)?;
                    }
                }
                Geometry::Rect(_) | Geometry::Triangle(_) => {
                    // Skip validation for these rarely used types
                }
            }

            Ok(())
        }
    }

    // TryFrom implementations for convenient conversions

    impl TryFrom<&str> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(wkt: &str) -> Result<Self, Self::Error> {
            Geography::from_wkt(wkt)
        }
    }

    impl TryFrom<String> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(wkt: String) -> Result<Self, Self::Error> {
            Geography::from_wkt(&wkt)
        }
    }

    impl TryFrom<Geometry<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(geometry: Geometry<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&geometry)
        }
    }

    impl TryFrom<&Geometry<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(geometry: &Geometry<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(geometry)
        }
    }

    impl TryFrom<geo_types::Point<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(point: geo_types::Point<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::Point(point))
        }
    }

    impl TryFrom<geo_types::LineString<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(line: geo_types::LineString<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::LineString(line))
        }
    }

    impl TryFrom<geo_types::Polygon<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(polygon: geo_types::Polygon<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::Polygon(polygon))
        }
    }

    impl TryFrom<geo_types::MultiPoint<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(mp: geo_types::MultiPoint<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::MultiPoint(mp))
        }
    }

    impl TryFrom<geo_types::MultiLineString<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(mls: geo_types::MultiLineString<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::MultiLineString(mls))
        }
    }

    impl TryFrom<geo_types::MultiPolygon<f64>> for Geography {
        type Error = Box<dyn Error + Send + Sync>;

        fn try_from(mp: geo_types::MultiPolygon<f64>) -> Result<Self, Self::Error> {
            Geography::from_geometry(&Geometry::MultiPolygon(mp))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn geography_format_wkb_detected() {
            let wkb_geo = Geography::from_wkt("POINT(1 2)").expect("wkt to wkb");
            match wkb_geo.format().expect("format detection") {
                GeographyFormat::Wkb(bytes) => {
                    assert!(!bytes.is_empty());
                    assert!(bytes[0] == 0 || bytes[0] == 1);
                }
                GeographyFormat::Legacy(_) => panic!("expected WKB format"),
            }
        }

        #[test]
        fn geography_format_legacy_detected() {
            let legacy_like = Geography::from_bytes(vec![0xFF, 0x00, 0x01, 0x02, 0x03]);
            match legacy_like.format().expect("format detection") {
                GeographyFormat::Legacy(bytes) => assert_eq!(bytes.len(), 5),
                GeographyFormat::Wkb(_) => panic!("expected Legacy format"),
            }
        }
    }
}

pub use geo_impl::GeoError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_roundtrip() {
        let date = Date::new(2024, 6, 15);
        let (y, m, d) = date.to_ymd();
        assert_eq!((y, m, d), (2024, 6, 15));
    }

    #[test]
    fn test_date_try_decode_valid() {
        // 2000-01-01 (Julian day 2451545)
        let date = Date::try_decode(2451545).unwrap();
        assert_eq!(date.days(), 0);
        let (y, m, d) = date.to_ymd();
        assert_eq!((y, m, d), (2000, 1, 1));
    }

    #[test]
    fn test_date_try_decode_year_1() {
        // Year 1, Jan 1 should be valid
        let result = Date::try_decode(Date::MIN_JULIAN_DAY as u32);
        assert!(result.is_ok());
    }

    #[test]
    fn test_date_try_decode_year_9999() {
        // Year 9999, Dec 31 should be valid
        let result = Date::try_decode(Date::MAX_JULIAN_DAY as u32);
        assert!(result.is_ok());
    }

    #[test]
    fn test_date_try_decode_invalid_too_small() {
        // Julian day 0 is way before year 1
        let result = Date::try_decode(0);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("out of supported range"));
    }

    #[test]
    fn test_date_try_decode_invalid_too_large() {
        // Very large Julian day (beyond year 9999)
        let result = Date::try_decode(u32::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_time_roundtrip() {
        let time = Time::new(14, 30, 45, 123456);
        let (h, m, s, us) = time.to_hms_micro();
        assert_eq!((h, m, s, us), (14, 30, 45, 123456));
    }

    #[test]
    fn test_numeric_display() {
        let num = Numeric::new(12345, 2);
        assert_eq!(num.to_string(), "123.45");
    }

    #[test]
    fn test_numeric_display_negative_sign_preserved() {
        // Regression: values in (-1, 0) must keep their sign. The integer
        // part is 0 for these, which previously dropped the minus sign and
        // rendered -0.5000 as "0.5000".
        assert_eq!(Numeric::new(-5000, 4).to_string(), "-0.5000");
        assert_eq!(Numeric::new(-9990, 4).to_string(), "-0.9990");
        assert_eq!(Numeric::new(-1, 4).to_string(), "-0.0001");

        // |value| >= 1 already worked; guard against regressions.
        assert_eq!(Numeric::new(-15000, 4).to_string(), "-1.5000");
        assert_eq!(Numeric::new(-10000, 4).to_string(), "-1.0000");

        // Zero and positive sub-unit values must not gain a spurious sign.
        assert_eq!(Numeric::new(0, 4).to_string(), "0.0000");
        assert_eq!(Numeric::new(5000, 4).to_string(), "0.5000");

        // scale == 0 path keeps negative integers intact.
        assert_eq!(Numeric::new(-1, 0).to_string(), "-1");

        // i128::MIN must not panic (unsigned_abs avoids the .abs() overflow).
        let _ = Numeric::new(i128::MIN, 4).to_string();
    }

    #[test]
    fn test_numeric_from_binary_with_scale() {
        // Unscaled value 123 with scale 2 = 1.23
        let mut bytes = [0u8; 16];
        bytes[0] = 123;

        let numeric = Numeric::from_binary_with_scale(&bytes, 2).unwrap();
        assert_eq!(numeric.unscaled_value(), 123);
        assert_eq!(numeric.scale(), 2);
        assert!((numeric.to_f64() - 1.23).abs() < 0.001);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "42.0 is exactly representable; scale=0 conversion must be bit-exact"
    )]
    fn test_numeric_from_binary_with_scale_zero() {
        // Unscaled value 42 with scale 0 = 42
        let mut bytes = [0u8; 16];
        bytes[0] = 42;

        let numeric = Numeric::from_binary_with_scale(&bytes, 0).unwrap();
        assert_eq!(numeric.unscaled_value(), 42);
        assert_eq!(numeric.scale(), 0);
        assert_eq!(numeric.to_f64(), 42.0);
    }

    #[test]
    fn test_numeric_from_binary_invalid_size() {
        // 4 bytes isn't one of Hyper's NUMERIC wire forms. 8 and 16 are
        // accepted (see tests below); anything else is an error.
        let bytes = [0u8; 4];
        let result = Numeric::from_binary_with_scale(&bytes, 2);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("invalid buffer size"), "got: {msg}");
        assert!(
            msg.contains("got 4"),
            "error should name the bad size; got: {msg}"
        );
    }

    /// The 8-byte wire form is used when precision ≤ 18 (Hyper's
    /// `Type::Numeric`). This is what `AVG(INTEGER)` returns as
    /// `Numeric(16, 6)` — the case that motivated this fix.
    #[test]
    fn test_numeric_from_binary_8_byte_form_avg_integer() {
        // Simulate `AVG(happiness_score)` where happiness_score is INT
        // and the observed average is 7.7. Hyper encodes this as
        // i64 = 7_700_000 with implicit scale 6, little-endian.
        let bytes: [u8; 8] = 7_700_000i64.to_le_bytes();

        let numeric = Numeric::from_binary_with_scale(&bytes, 6).unwrap();
        assert_eq!(numeric.unscaled_value(), 7_700_000);
        assert_eq!(numeric.scale(), 6);
        assert!((numeric.to_f64() - 7.7).abs() < 1e-9);
        assert_eq!(numeric.to_string(), "7.700000");
    }

    /// 8-byte decode should round-trip: encode → decode yields the same
    /// value as long as the value fits in i64 (precision ≤ 18).
    #[test]
    fn test_numeric_decode_int64_roundtrip() {
        let original = Numeric::new(12_345_678_901_234i128, 4);
        let bytes = original.encode_int64().expect("value fits in i64");
        let decoded = Numeric::decode_int64(&bytes, 4);
        assert_eq!(original, decoded);
    }

    /// `encode_int64` must refuse to silently truncate when the value
    /// won't fit in an `i64`. Regression guard for the
    /// originally-shipped `as i64` cast, which wrapped silently in
    /// release builds and would have corrupted wire data without any
    /// observable symptom until the server saw a bogus value.
    #[test]
    fn test_numeric_encode_int64_rejects_overflow() {
        // 10^19 > i64::MAX (~9.22 × 10^18). The `as i64` cast would
        // have wrapped this to a negative number; try_from gives us
        // an error instead.
        let too_big = Numeric::new(10_000_000_000_000_000_000i128, 0);
        let err = too_big
            .encode_int64()
            .expect_err("10^19 must not encode as i64");
        let msg = err.to_string();
        assert!(msg.contains("outside i64 range"), "got: {msg}");
        assert!(
            msg.contains("10000000000000000000"),
            "error should name the offending value; got: {msg}"
        );

        // Negative overflow — just as dangerous (wraps to a large
        // positive on truncation).
        let too_small = Numeric::new(-10_000_000_000_000_000_000i128, 0);
        assert!(too_small.encode_int64().is_err());

        // Boundary: i64::MAX itself fits exactly.
        let at_max = Numeric::new(i128::from(i64::MAX), 0);
        let bytes = at_max.encode_int64().expect("i64::MAX fits");
        let roundtrip = Numeric::decode_int64(&bytes, 0);
        assert_eq!(roundtrip.unscaled_value(), i128::from(i64::MAX));

        // Boundary: i64::MIN itself fits exactly.
        let at_min = Numeric::new(i128::from(i64::MIN), 0);
        let bytes = at_min.encode_int64().expect("i64::MIN fits");
        let roundtrip = Numeric::decode_int64(&bytes, 0);
        assert_eq!(roundtrip.unscaled_value(), i128::from(i64::MIN));
    }

    /// Directly exercise `decode_int64` against the exact byte pattern
    /// we saw on the wire: `avg_coffee = 2.0` was encoded as
    /// `[80, 84, 1e, 00, 00, 00, 00, 00]` = 2_000_000 with scale 6.
    /// Locking this specific pattern in regresses any future break of
    /// the 8-byte decode path.
    #[test]
    fn test_numeric_decode_int64_wire_pattern_from_hyperd() {
        let bytes: [u8; 8] = [0x80, 0x84, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00];
        let numeric = Numeric::decode_int64(&bytes, 6);
        assert_eq!(numeric.unscaled_value(), 2_000_000);
        assert_eq!(numeric.scale(), 6);
        assert!((numeric.to_f64() - 2.0).abs() < 1e-12);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "123.0 is exactly representable; scale=0 conversion must be bit-exact"
    )]
    fn test_numeric_from_binary_with_scale_required() {
        // FromHyperBinary is intentionally NOT implemented for Numeric because
        // the scale must be obtained from type metadata - it cannot be inferred
        // from the binary data alone. Use from_binary_with_scale instead.
        let mut bytes = [0u8; 16];
        bytes[0] = 123; // Unscaled value

        // Correct approach: use from_binary_with_scale with scale from metadata
        // For NUMERIC(10, 2) column, scale = 2
        let numeric = Numeric::from_binary_with_scale(&bytes, 2).unwrap();
        assert_eq!(numeric.scale(), 2);
        assert!((numeric.to_f64() - 1.23).abs() < 0.001);

        // For NUMERIC(10, 0) column (integer-like), scale = 0
        let numeric_int = Numeric::from_binary_with_scale(&bytes, 0).unwrap();
        assert_eq!(numeric_int.scale(), 0);
        assert_eq!(numeric_int.to_f64(), 123.0);
    }

    #[test]
    fn test_interval_encode_decode() {
        let interval = Interval::new(14, 3, 3661000000); // 1 year 2 months 3 days 1:01:01
        let encoded = interval.encode();
        let decoded = Interval::decode(&encoded);
        assert_eq!(interval, decoded);
    }

    #[test]
    fn test_little_endian_i32() {
        let mut buf = BytesMut::new();
        42i32.to_hyper_binary_not_null(&mut buf).unwrap();
        assert_eq!(buf.as_ref(), &[42, 0, 0, 0]); // LittleEndian
    }

    // -----------------------------------------------------------------
    // Date::try_from_days
    // -----------------------------------------------------------------

    #[test]
    fn date_try_from_days_accepts_min() {
        assert!(Date::try_from_days(Date::MIN_DAYS).is_ok());
    }

    #[test]
    fn date_try_from_days_accepts_max() {
        assert!(Date::try_from_days(Date::MAX_DAYS).is_ok());
    }

    #[test]
    fn date_try_from_days_accepts_epoch() {
        let d = Date::try_from_days(0).unwrap();
        assert_eq!(d.days(), 0);
    }

    #[test]
    fn date_try_from_days_rejects_below_min() {
        let err = Date::try_from_days(Date::MIN_DAYS - 1).expect_err("expected range error");
        let msg = err.to_string();
        assert!(msg.contains("out of supported range"), "got: {msg}");
    }

    #[test]
    fn date_try_from_days_rejects_above_max() {
        assert!(Date::try_from_days(Date::MAX_DAYS + 1).is_err());
    }

    #[test]
    fn date_try_from_days_rejects_extreme_values() {
        assert!(Date::try_from_days(i32::MAX).is_err());
        assert!(Date::try_from_days(i32::MIN).is_err());
    }

    #[test]
    fn date_max_min_days_invariant() {
        // Encode round-trip at boundaries — sanity check that MIN/MAX
        // produce a non-trivial range. The exact ymd at the boundaries
        // depends on whether the algorithm uses proleptic-Gregorian with
        // astronomical year zero; we just verify monotonicity here.
        let min = Date::try_from_days(Date::MIN_DAYS).unwrap();
        let max = Date::try_from_days(Date::MAX_DAYS).unwrap();
        assert!(min.days() < max.days());
        // Max is around year 9999.
        assert!(max.to_ymd().0 >= 9999);
    }

    // -----------------------------------------------------------------
    // Time::try_from_microseconds
    // -----------------------------------------------------------------

    #[test]
    fn time_try_from_microseconds_accepts_zero() {
        let t = Time::try_from_microseconds(0).unwrap();
        assert_eq!(t.microseconds(), 0);
    }

    #[test]
    fn time_try_from_microseconds_accepts_max() {
        let t = Time::try_from_microseconds(Time::MAX_MICROSECONDS).unwrap();
        let (h, m, s, _) = t.to_hms_micro();
        assert_eq!((h, m, s), (23, 59, 59));
    }

    #[test]
    fn time_try_from_microseconds_max_is_23_59_59_999999() {
        // 24h - 1us
        assert_eq!(Time::MAX_MICROSECONDS, 86_399_999_999);
    }

    #[test]
    fn time_try_from_microseconds_rejects_24_hours_exact() {
        // Exactly 24h is invalid
        assert!(Time::try_from_microseconds(86_400_000_000).is_err());
    }

    #[test]
    fn time_try_from_microseconds_rejects_extreme_values() {
        assert!(Time::try_from_microseconds(u64::MAX).is_err());
    }

    #[test]
    fn time_from_hyper_binary_rejects_out_of_range() {
        // Wire-data path: hostile server sends >= 24h microseconds value.
        let bytes = (86_400_000_000u64).to_le_bytes();
        let err = <Time as FromHyperBinary>::from_hyper_binary(&bytes).unwrap_err();
        assert!(err.to_string().contains("24-hour"));
    }

    // -----------------------------------------------------------------
    // Numeric::try_from_f64
    // -----------------------------------------------------------------

    #[test]
    fn numeric_try_from_f64_accepts_typical() {
        let n = Numeric::try_from_f64(1.23, 2).unwrap();
        assert!((n.to_f64() - 1.23).abs() < 1e-9);
    }

    #[test]
    fn numeric_try_from_f64_accepts_zero() {
        let n = Numeric::try_from_f64(0.0, 5).unwrap();
        assert!(n.to_f64().abs() < f64::EPSILON);
    }

    #[test]
    fn numeric_try_from_f64_accepts_negative() {
        let n = Numeric::try_from_f64(-99.99, 2).unwrap();
        assert!((n.to_f64() - -99.99).abs() < 1e-9);
    }

    #[test]
    fn numeric_try_from_f64_rejects_infinity() {
        let err = Numeric::try_from_f64(f64::INFINITY, 0).unwrap_err();
        assert!(err.to_string().contains("not finite"));
    }

    #[test]
    fn numeric_try_from_f64_rejects_nan_after_scale() {
        let err = Numeric::try_from_f64(f64::NAN, 0).unwrap_err();
        // NaN * multiplier is NaN, which is_finite returns false
        assert!(err.to_string().contains("not finite"));
    }

    #[test]
    fn numeric_try_from_f64_rejects_huge_value_with_honest_error() {
        // 1e50 way exceeds i128::MAX (~1.7e38) — the saturating-cast comment
        // says the error message should report the actual scaled value.
        let err = Numeric::try_from_f64(1e50, 0).unwrap_err();
        let msg = err.to_string();
        // Either the pre-cast i128-range check or the post-cast 38-digit check
        // catches it. Either is acceptable; we just want a clear error.
        assert!(
            msg.contains("exceeds") || msg.contains("range") || msg.contains("precision"),
            "expected an exceedance error, got: {msg}"
        );
    }

    #[test]
    fn numeric_try_from_f64_accepts_value_near_38_digit_boundary() {
        // 1e38 as f64 actually rounds DOWN to ~9.9999...e37 (below MAX_UNSCALED)
        // due to f64 mantissa precision. Verify this doesn't error — if a future
        // change tightens the bounds check above MAX_UNSCALED, this test catches
        // accidentally rejecting representable values near the boundary.
        let n = Numeric::try_from_f64(1e38, 0).unwrap();
        // Verify it landed under the cap, not silently saturated to MAX.
        let unscaled = n.unscaled_value();
        assert!(unscaled.abs() < 10_i128.pow(38));
    }

    #[test]
    fn numeric_try_from_f64_rejects_above_38_digit_boundary() {
        // 1e39 is ~10x the cap and is solidly out of range.
        assert!(Numeric::try_from_f64(1e39, 0).is_err());
    }

    #[test]
    fn numeric_from_f64_panics_on_huge_value() {
        let result = std::panic::catch_unwind(|| Numeric::from_f64(1e50, 0));
        assert!(result.is_err(), "from_f64 should panic for out-of-range");
    }
}
