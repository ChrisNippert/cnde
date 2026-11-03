//! Things that follow the time of day: the dark style (`[dark_mode]`) can
//! switch at sunset and sunrise, or at set times.
//!
//! Sunset and sunrise are worked out here from `[location]`, with NOAA's
//! solar position equations: nothing is looked up online. Without a
//! location, "sunset" falls back to the custom times.
//!
//! Functions of a [`Clock`], so the compositor (which acts on them) and
//! Settings (which shows them) agree, and the tests need no real time.

use serde::Deserialize;

use crate::{Config, Error};

/// `[dark_mode]`: switching `appearance.color_scheme` by itself.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DarkMode {
    pub schedule: DarkSchedule,
    /// When the dark style starts and ends ("HH:MM", 24-hour), for
    /// `schedule = "custom"`, and for "sunset" until a location is set.
    pub from: String,
    pub to: String,
}

impl Default for DarkMode {
    fn default() -> Self {
        Self {
            schedule: DarkSchedule::Off,
            from: "19:00".into(),
            to: "07:00".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DarkSchedule {
    /// Only switched by hand.
    #[default]
    Off,
    /// Dark from sunset to sunrise.
    Sunset,
    /// Dark from `from` to `to`.
    Custom,
}

/// `[location]`: where sunset and sunrise are worked out for.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Location {
    /// "latitude, longitude" in degrees, north and east positive, e.g.
    /// "52.52, 13.40". Empty: unknown.
    pub coordinates: String,
}

impl Location {
    /// (latitude, longitude), if set and valid.
    pub fn coordinates(&self) -> Option<(f64, f64)> {
        parse_coordinates(&self.coordinates)
    }
}

/// "52.52, 13.40" into (52.52, 13.40); out-of-range values are rejected.
pub fn parse_coordinates(text: &str) -> Option<(f64, f64)> {
    let (lat, lon) = text.split_once(',')?;
    let lat: f64 = lat.trim().parse().ok()?;
    let lon: f64 = lon.trim().parse().ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// "HH:MM" (24-hour; "7:05" is fine too) into minutes after midnight.
pub fn parse_time(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Minutes after midnight as "HH:MM".
pub fn format_time(minutes: f64) -> String {
    let total = (minutes.round() as i64).rem_euclid(24 * 60);
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// The local date and time a schedule is checked against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clock {
    pub year: i32,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    /// Minutes since local midnight.
    pub minute: f64,
    /// Local time minus UTC, in minutes (e.g. 120 for UTC+2).
    pub utc_offset: i32,
}

impl Clock {
    /// The local time now.
    pub fn now() -> Self {
        use chrono::{Datelike, Offset, Timelike};
        let now = chrono::Local::now();
        Self {
            year: now.year(),
            month: now.month(),
            day: now.day(),
            minute: now.hour() as f64 * 60.0 + now.minute() as f64 + now.second() as f64 / 60.0,
            utc_offset: now.offset().fix().local_minus_utc() / 60,
        }
    }
}

/// The sun's day at one place, in local minutes after midnight. Either can
/// fall outside 0..1440 when the time zone is far from the sun's (e.g.
/// western China); [`When`] wraps them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sun {
    Rises {
        sunrise: f64,
        sunset: f64,
    },
    /// Midnight sun: it doesn't set today.
    AlwaysUp,
    /// Polar night: it doesn't rise today.
    AlwaysDown,
}

/// Sunrise and sunset on `clock`'s date at (`lat`, `lon`), from NOAA's
/// solar calculator (the "General Solar Position Calculations" paper and
/// its spreadsheet). Good to a minute or so between the polar circles.
pub fn sun(lat: f64, lon: f64, clock: &Clock) -> Sun {
    // Julian centuries since J2000.0, at local noon.
    let jd =
        julian_day(clock.year, clock.month, clock.day) + 0.5 - clock.utc_offset as f64 / 1440.0;
    let t = (jd - 2451545.0) / 36525.0;

    let mean_long = (280.46646 + t * (36000.76983 + t * 0.0003032)).rem_euclid(360.0);
    let mean_anomaly = 357.52911 + t * (35999.05029 - 0.0001537 * t);
    let eccentricity = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let m = mean_anomaly.to_radians();
    let center = m.sin() * (1.914602 - t * (0.004817 + 0.000014 * t))
        + (2.0 * m).sin() * (0.019993 - 0.000101 * t)
        + (3.0 * m).sin() * 0.000289;
    let true_long = mean_long + center;
    let omega = (125.04 - 1934.136 * t).to_radians();
    let apparent_long = (true_long - 0.00569 - 0.00478 * omega.sin()).to_radians();
    let mean_obliquity =
        23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0;
    let obliquity = (mean_obliquity + 0.00256 * omega.cos()).to_radians();
    let declination = (obliquity.sin() * apparent_long.sin()).asin();

    // Equation of time, minutes: how far the sun runs ahead of the clock.
    let y = (obliquity / 2.0).tan().powi(2);
    let l0 = mean_long.to_radians();
    let e = eccentricity;
    let equation_of_time = 4.0
        * (y * (2.0 * l0).sin() - 2.0 * e * m.sin() + 4.0 * e * y * m.sin() * (2.0 * l0).cos()
            - 0.5 * y * y * (4.0 * l0).sin()
            - 1.25 * e * e * (2.0 * m).sin())
        .to_degrees();

    // The hour angle at which the sun's top edge meets the horizon,
    // refraction included (the sun's centre at 90.833° from overhead).
    let lat = lat.to_radians();
    let cos_hour_angle = 90.833f64.to_radians().cos() / (lat.cos() * declination.cos())
        - lat.tan() * declination.tan();
    if cos_hour_angle > 1.0 {
        return Sun::AlwaysDown;
    }
    if cos_hour_angle < -1.0 {
        return Sun::AlwaysUp;
    }
    let hour_angle = cos_hour_angle.acos().to_degrees();
    let noon = 720.0 - 4.0 * lon - equation_of_time + clock.utc_offset as f64;
    Sun::Rises {
        sunrise: noon - 4.0 * hour_angle,
        sunset: noon + 4.0 * hour_angle,
    }
}

/// The Julian day number at 0:00 UTC on a Gregorian date.
fn julian_day(year: i32, month: u32, day: u32) -> f64 {
    let (mut y, mut m) = (year as f64, month as f64);
    if month <= 2 {
        y -= 1.0;
        m += 12.0;
    }
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (y + 4716.0)).floor() + (30.6001 * (m + 1.0)).floor() + day as f64 + b - 1524.5
}

/// When during the day something is on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    Always,
    Never,
    /// From `from` up to (not including) `to`, in local minutes; a span
    /// that ends before it starts runs over midnight. Equal ends: never.
    Between {
        from: f64,
        to: f64,
    },
}

impl When {
    pub fn contains(&self, minute: f64) -> bool {
        match *self {
            When::Always => true,
            When::Never => false,
            When::Between { from, to } => {
                let day = 24.0 * 60.0;
                let (from, to, now) = (
                    from.rem_euclid(day),
                    to.rem_euclid(day),
                    minute.rem_euclid(day),
                );
                if from <= to {
                    from <= now && now < to
                } else {
                    now >= from || now < to
                }
            }
        }
    }

    /// From sunset to sunrise at `location` on `clock`'s date; `None`
    /// without a location.
    pub fn night(location: &Location, clock: &Clock) -> Option<When> {
        let (lat, lon) = location.coordinates()?;
        Some(match sun(lat, lon, clock) {
            Sun::Rises { sunrise, sunset } => When::Between {
                from: sunset,
                to: sunrise,
            },
            Sun::AlwaysUp => When::Never,
            Sun::AlwaysDown => When::Always,
        })
    }

    /// From one "HH:MM" to another; `Never` if either doesn't parse
    /// (validation keeps that from happening).
    pub fn custom(from: &str, to: &str) -> When {
        match (parse_time(from), parse_time(to)) {
            (Some(from), Some(to)) => When::Between {
                from: from as f64,
                to: to as f64,
            },
            _ => When::Never,
        }
    }
}

impl DarkMode {
    /// When the schedule wants the dark style; `None` when it's off.
    pub fn when(&self, location: &Location, clock: &Clock) -> Option<When> {
        match self.schedule {
            DarkSchedule::Off => None,
            DarkSchedule::Sunset => Some(
                When::night(location, clock).unwrap_or_else(|| When::custom(&self.from, &self.to)),
            ),
            DarkSchedule::Custom => Some(When::custom(&self.from, &self.to)),
        }
    }

    /// Whether the schedule wants the dark style now; `None` when it's off.
    pub fn wants_dark(&self, location: &Location, clock: &Clock) -> Option<bool> {
        self.when(location, clock)
            .map(|when| when.contains(clock.minute))
    }
}

/// `[night_light]`: a warm colour filter over the screen.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NightLight {
    pub schedule: NightSchedule,
    /// Colour temperature in kelvin: lower is warmer.
    pub temperature: u32,
    /// How strongly the filter applies, in percent (0 is none).
    pub intensity: u8,
    /// When it starts and ends ("HH:MM", 24-hour), for `schedule =
    /// "custom"`, and for "sunset" until a location is set.
    pub from: String,
    pub to: String,
}

impl Default for NightLight {
    fn default() -> Self {
        Self {
            schedule: NightSchedule::Off,
            temperature: 4000,
            intensity: 100,
            from: "20:00".into(),
            to: "06:00".into(),
        }
    }
}

/// The warmest and coolest temperatures accepted (6500 K is no change).
pub const NIGHT_LIGHT_RANGE: std::ops::RangeInclusive<u32> = 2500..=6500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NightSchedule {
    #[default]
    Off,
    /// On all the time.
    Always,
    /// From sunset to sunrise.
    Sunset,
    /// From `from` to `to`.
    Custom,
}

impl NightLight {
    /// The temperature to filter at on `clock`, or `None` when it's off
    /// (or not its time).
    pub fn target(&self, location: &Location, clock: &Clock) -> Option<u32> {
        let when = match self.schedule {
            NightSchedule::Off => When::Never,
            NightSchedule::Always => When::Always,
            NightSchedule::Sunset => {
                When::night(location, clock).unwrap_or_else(|| When::custom(&self.from, &self.to))
            }
            NightSchedule::Custom => When::custom(&self.from, &self.to),
        };
        when.contains(clock.minute).then_some(self.temperature)
    }
}

/// The checks `Config::validate` runs on these sections.
pub(crate) fn validate(config: &Config) -> Result<(), Error> {
    if !NIGHT_LIGHT_RANGE.contains(&config.night_light.temperature) {
        return Err(Error::Invalid(format!(
            "night_light.temperature must be between {} and {} kelvin, got {}",
            NIGHT_LIGHT_RANGE.start(),
            NIGHT_LIGHT_RANGE.end(),
            config.night_light.temperature
        )));
    }
    if config.night_light.intensity > 100 {
        return Err(Error::Invalid(format!(
            "night_light.intensity must be between 0 and 100 percent, got {}",
            config.night_light.intensity
        )));
    }
    for (name, text) in [
        ("dark_mode.from", &config.dark_mode.from),
        ("dark_mode.to", &config.dark_mode.to),
        ("night_light.from", &config.night_light.from),
        ("night_light.to", &config.night_light.to),
    ] {
        if parse_time(text).is_none() {
            return Err(Error::Invalid(format!(
                "{name} must be a 24-hour time like \"19:30\", got \"{text}\""
            )));
        }
    }
    let coordinates = &config.location.coordinates;
    if !coordinates.trim().is_empty() && parse_coordinates(coordinates).is_none() {
        return Err(Error::Invalid(format!(
            "location.coordinates must be \"latitude, longitude\" in degrees \
             (e.g. \"52.52, 13.40\"), got \"{coordinates}\""
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(year: i32, month: u32, day: u32, utc_offset: i32) -> Clock {
        Clock {
            year,
            month,
            day,
            minute: 0.0,
            utc_offset,
        }
    }

    /// `hh:mm` as minutes, for readable expectations.
    fn at(text: &str) -> f64 {
        parse_time(text).unwrap() as f64
    }

    fn assert_sun(sun: Sun, sunrise: &str, sunset: &str) {
        let Sun::Rises {
            sunrise: rise,
            sunset: set,
        } = sun
        else {
            panic!("expected a sunrise and sunset, got {sun:?}");
        };
        // Published tables round to the minute and use slightly different
        // refraction; two minutes either way is the same answer.
        assert!(
            (rise - at(sunrise)).abs() <= 2.0,
            "sunrise {} != {sunrise}",
            format_time(rise)
        );
        assert!(
            (set - at(sunset)).abs() <= 2.0,
            "sunset {} != {sunset}",
            format_time(set)
        );
    }

    #[test]
    fn julian_days() {
        // J2000.0 is noon on 2000-01-01.
        assert_eq!(julian_day(2000, 1, 1), 2451544.5);
        assert_eq!(julian_day(1999, 12, 31), 2451543.5);
        assert_eq!(julian_day(2024, 3, 1), 2460370.5);
    }

    #[test]
    fn sunrise_and_sunset() {
        // London, midsummer (BST, UTC+1): 04:43 and 21:21.
        assert_sun(
            sun(51.5074, -0.1278, &clock(2024, 6, 21, 60)),
            "04:43",
            "21:21",
        );
        // New York, midwinter (EST, UTC-5): 07:16 and 16:32.
        assert_sun(
            sun(40.7128, -74.0060, &clock(2024, 12, 21, -300)),
            "07:16",
            "16:32",
        );
        // Sydney, New Year's Day (AEDT, UTC+11): 05:48 and 20:09.
        assert_sun(
            sun(-33.8688, 151.2093, &clock(2025, 1, 1, 660)),
            "05:48",
            "20:09",
        );
        // The equator: about twelve hours of day all year.
        let Sun::Rises { sunrise, sunset } = sun(0.0, 0.0, &clock(2024, 3, 20, 0)) else {
            panic!("the sun rises at the equator");
        };
        assert!((sunset - sunrise - 12.0 * 60.0).abs() < 10.0);
    }

    #[test]
    fn polar_days_and_nights() {
        // Tromsø: midnight sun in June, polar night in December.
        assert_eq!(
            sun(69.6492, 18.9553, &clock(2024, 6, 21, 120)),
            Sun::AlwaysUp
        );
        assert_eq!(
            sun(69.6492, 18.9553, &clock(2024, 12, 21, 60)),
            Sun::AlwaysDown
        );
        let tromso = Location {
            coordinates: "69.6492, 18.9553".into(),
        };
        assert_eq!(
            When::night(&tromso, &clock(2024, 6, 21, 120)),
            Some(When::Never)
        );
        assert_eq!(
            When::night(&tromso, &clock(2024, 12, 21, 60)),
            Some(When::Always)
        );
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("19:00"), Some(19 * 60));
        assert_eq!(parse_time("7:05"), Some(7 * 60 + 5));
        assert_eq!(parse_time(" 00:00 "), Some(0));
        assert_eq!(parse_time("23:59"), Some(23 * 60 + 59));
        for bad in ["24:00", "12:60", "1900", "7:5", "", ":30", "ab:cd", "-1:00"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
        assert_eq!(format_time(at("07:05")), "07:05");
        assert_eq!(format_time(-30.0), "23:30");
        assert_eq!(format_time(1440.0 + 61.0), "01:01");
    }

    #[test]
    fn coordinates_parse() {
        assert_eq!(parse_coordinates("52.52, 13.40"), Some((52.52, 13.4)));
        assert_eq!(parse_coordinates("-33.9,151.2"), Some((-33.9, 151.2)));
        assert_eq!(parse_coordinates(""), None);
        assert_eq!(parse_coordinates("52.52"), None);
        assert_eq!(parse_coordinates("91, 0"), None);
        assert_eq!(parse_coordinates("0, 181"), None);
    }

    #[test]
    fn spans() {
        let day = When::custom("07:00", "19:00");
        assert!(!day.contains(at("06:59")));
        assert!(day.contains(at("07:00")));
        assert!(day.contains(at("18:59")));
        assert!(!day.contains(at("19:00")));

        // Over midnight.
        let night = When::custom("19:00", "07:00");
        assert!(night.contains(at("19:00")));
        assert!(night.contains(at("23:59")));
        assert!(night.contains(at("00:00")));
        assert!(night.contains(at("06:59")));
        assert!(!night.contains(at("07:00")));
        assert!(!night.contains(at("12:00")));

        // Sun times outside the day wrap round.
        let late = When::Between {
            from: 22.0 * 60.0,
            to: 24.0 * 60.0 + 30.0,
        };
        assert!(late.contains(at("00:15")));
        assert!(!late.contains(at("00:45")));

        assert!(!When::custom("08:00", "08:00").contains(at("08:00")));
        assert!(When::Always.contains(0.0));
        assert!(!When::Never.contains(0.0));
    }

    #[test]
    fn dark_mode_schedules() {
        let mut dark = DarkMode::default();
        let nowhere = Location::default();
        let berlin = Location {
            coordinates: "52.52, 13.40".into(),
        };
        let mut now = clock(2024, 6, 21, 120);
        now.minute = at("20:00");
        assert_eq!(dark.wants_dark(&berlin, &now), None, "off by default");

        dark.schedule = DarkSchedule::Custom;
        assert_eq!(dark.wants_dark(&berlin, &now), Some(true));
        now.minute = at("12:00");
        assert_eq!(dark.wants_dark(&berlin, &now), Some(false));

        // Berlin's midsummer sun sets around 21:33.
        dark.schedule = DarkSchedule::Sunset;
        now.minute = at("20:00");
        assert_eq!(dark.wants_dark(&berlin, &now), Some(false));
        now.minute = at("22:00");
        assert_eq!(dark.wants_dark(&berlin, &now), Some(true));
        now.minute = at("04:00");
        assert_eq!(dark.wants_dark(&berlin, &now), Some(true));
        now.minute = at("06:00");
        assert_eq!(dark.wants_dark(&berlin, &now), Some(false));
        // No location: the custom times (19:00 to 07:00) stand in.
        now.minute = at("20:00");
        assert_eq!(dark.wants_dark(&nowhere, &now), Some(true));
    }

    #[test]
    fn night_light_schedules() {
        let mut light = NightLight::default();
        let nowhere = Location::default();
        let mut now = clock(2024, 6, 21, 120);
        now.minute = at("23:00");
        assert_eq!(light.target(&nowhere, &now), None, "off by default");

        light.schedule = NightSchedule::Always;
        light.temperature = 3500;
        now.minute = at("12:00");
        assert_eq!(light.target(&nowhere, &now), Some(3500));

        // 20:00 to 06:00 runs over midnight.
        light.schedule = NightSchedule::Custom;
        for (time, on) in [
            ("19:59", false),
            ("20:00", true),
            ("00:00", true),
            ("05:59", true),
            ("06:00", false),
        ] {
            now.minute = at(time);
            assert_eq!(light.target(&nowhere, &now).is_some(), on, "{time}");
        }

        // Without a location, sunset uses the custom times.
        light.schedule = NightSchedule::Sunset;
        now.minute = at("21:00");
        assert!(light.target(&nowhere, &now).is_some());
        // Berlin's midsummer sun sets around 21:33.
        let berlin = Location {
            coordinates: "52.52, 13.40".into(),
        };
        assert_eq!(light.target(&berlin, &now), None);
        now.minute = at("22:00");
        assert_eq!(light.target(&berlin, &now), Some(3500));
    }

    #[test]
    fn night_light_validation() {
        assert!(Config::parse("[night_light]\ntemperature = 1000\n").is_err());
        assert!(Config::parse("[night_light]\ntemperature = 7000\n").is_err());
        assert!(Config::parse("[night_light]\nintensity = 101\n").is_err());
        assert_eq!(Config::default().night_light.intensity, 100);
        assert!(Config::parse("[night_light]\nfrom = \"late\"\n").is_err());
        let config =
            Config::parse("[night_light]\nschedule = \"sunset\"\ntemperature = 2500\n").unwrap();
        assert_eq!(config.night_light.schedule, NightSchedule::Sunset);
        assert_eq!(config.night_light.temperature, 2500);
    }

    #[test]
    fn validation() {
        assert!(Config::parse("[dark_mode]\nfrom = \"25:00\"\n").is_err());
        assert!(Config::parse("[location]\ncoordinates = \"somewhere\"\n").is_err());
        let config = Config::parse(
            "[dark_mode]\nschedule = \"sunset\"\nfrom = \"21:30\"\nto = \"6:45\"\n\
             [location]\ncoordinates = \"52.52, 13.40\"\n",
        )
        .unwrap();
        assert_eq!(config.dark_mode.schedule, DarkSchedule::Sunset);
        assert_eq!(config.location.coordinates(), Some((52.52, 13.4)));
    }
}
