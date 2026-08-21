use std::fmt;
use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, Months, NaiveDate, TimeZone, Timelike, Utc,
    Weekday,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleWeekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl ScheduleWeekday {
    const fn chrono(self) -> Weekday {
        match self {
            Self::Monday => Weekday::Mon,
            Self::Tuesday => Weekday::Tue,
            Self::Wednesday => Weekday::Wed,
            Self::Thursday => Weekday::Thu,
            Self::Friday => Weekday::Fri,
            Self::Saturday => Weekday::Sat,
            Self::Sunday => Weekday::Sun,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim_matches(|character: char| !character.is_ascii_alphabetic()) {
            "monday" | "mondays" | "mon" => Some(Self::Monday),
            "tuesday" | "tuesdays" | "tue" | "tues" => Some(Self::Tuesday),
            "wednesday" | "wednesdays" | "wed" => Some(Self::Wednesday),
            "thursday" | "thursdays" | "thu" | "thur" | "thurs" => Some(Self::Thursday),
            "friday" | "fridays" | "fri" => Some(Self::Friday),
            "saturday" | "saturdays" | "sat" => Some(Self::Saturday),
            "sunday" | "sundays" | "sun" => Some(Self::Sunday),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationSchedule {
    IntervalMinutes {
        minutes: u32,
    },
    Daily {
        minute_of_day: u16,
    },
    Weekly {
        weekdays: Vec<ScheduleWeekday>,
        minute_of_day: u16,
    },
    Monthly {
        day: u8,
        minute_of_day: u16,
    },
    Yearly {
        month: u8,
        day: u8,
        minute_of_day: u16,
    },
}

impl AutomationSchedule {
    /// Return the first occurrence strictly after `after`, interpreted in the
    /// machine's local timezone for calendar schedules.
    #[must_use]
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::IntervalMinutes { minutes } => {
                let candidate = after + Duration::minutes(i64::from(*minutes));
                let seconds_into_step =
                    i64::from(candidate.minute() % 5) * 60 + i64::from(candidate.second());
                let rounded = if seconds_into_step == 0 && candidate.nanosecond() == 0 {
                    candidate
                } else {
                    candidate + Duration::seconds(300 - seconds_into_step)
                        - Duration::nanoseconds(i64::from(candidate.nanosecond()))
                };
                Some(rounded)
            }
            Self::Daily { minute_of_day } => {
                next_calendar_day(after, 370, |date| local_occurrence(date, *minute_of_day))
            }
            Self::Weekly {
                weekdays,
                minute_of_day,
            } => next_calendar_day(after, 14, |date| {
                weekdays
                    .iter()
                    .any(|weekday| weekday.chrono() == date.weekday())
                    .then(|| local_occurrence(date, *minute_of_day))
                    .flatten()
            }),
            Self::Monthly { day, minute_of_day } => {
                let local_after = after.with_timezone(&Local);
                let first = NaiveDate::from_ymd_opt(local_after.year(), local_after.month(), 1)?;
                (0..=24).find_map(|offset| {
                    let month = first.checked_add_months(Months::new(offset))?;
                    let next_month = month.checked_add_months(Months::new(1))?;
                    let last_day = next_month.checked_sub_signed(Duration::days(1))?.day();
                    let date = NaiveDate::from_ymd_opt(
                        month.year(),
                        month.month(),
                        u32::from(*day).min(last_day),
                    )?;
                    let candidate = local_occurrence(date, *minute_of_day)?;
                    (candidate > after).then_some(candidate)
                })
            }
            Self::Yearly {
                month,
                day,
                minute_of_day,
            } => {
                let start_year = after.with_timezone(&Local).year();
                (start_year..=start_year + 8).find_map(|year| {
                    let date = NaiveDate::from_ymd_opt(year, u32::from(*month), u32::from(*day))?;
                    let candidate = local_occurrence(date, *minute_of_day)?;
                    (candidate > after).then_some(candidate)
                })
            }
        }
    }
}

impl FromStr for AutomationSchedule {
    type Err = ScheduleParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_schedule(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduleParseError(String);

impl fmt::Display for ScheduleParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ScheduleParseError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Automation {
    pub id: String,
    pub name: String,
    pub schedule_description: String,
    pub schedule: AutomationSchedule,
    pub condition: Option<String>,
    pub instructions: String,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub mode: String,
    pub reasoning_effort: Option<String>,
    pub context_tier: Option<String>,
    pub project_path: Option<String>,
    pub enabled: bool,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationRunStatus {
    Running,
    Skipped,
    Succeeded,
    Failed,
}

impl AutomationRunStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Skipped => "skipped",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub fn from_str_or_running(value: &str) -> Self {
        match value {
            "skipped" => Self::Skipped,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            _ => Self::Running,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutomationRun {
    pub id: String,
    pub automation_id: String,
    pub automation_name: String,
    pub scheduled_for: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: AutomationRunStatus,
    pub condition_result: Option<bool>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub session_id: Option<String>,
}

fn parse_schedule(value: &str) -> Result<AutomationSchedule, ScheduleParseError> {
    let normalized = value
        .trim()
        .to_ascii_lowercase()
        .replace(',', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        return Err(schedule_help());
    }

    if let Some(interval) = parse_interval(&normalized)? {
        return Ok(interval);
    }

    let (cadence, minute_of_day) = split_time(&normalized)?;
    if matches!(cadence, "daily" | "every day" | "each day") {
        return Ok(AutomationSchedule::Daily { minute_of_day });
    }
    if matches!(
        cadence,
        "weekdays" | "every weekday" | "every weekdays" | "each weekday"
    ) {
        return Ok(AutomationSchedule::Weekly {
            weekdays: vec![
                ScheduleWeekday::Monday,
                ScheduleWeekday::Tuesday,
                ScheduleWeekday::Wednesday,
                ScheduleWeekday::Thursday,
                ScheduleWeekday::Friday,
            ],
            minute_of_day,
        });
    }
    if matches!(cadence, "weekends" | "every weekend" | "each weekend") {
        return Ok(AutomationSchedule::Weekly {
            weekdays: vec![ScheduleWeekday::Saturday, ScheduleWeekday::Sunday],
            minute_of_day,
        });
    }

    if let Some(monthly) = parse_monthly(cadence, minute_of_day)? {
        return Ok(monthly);
    }
    if let Some(yearly) = parse_yearly(cadence, minute_of_day)? {
        return Ok(yearly);
    }

    let mut weekdays = cadence
        .split_whitespace()
        .filter(|token| !matches!(*token, "every" | "each" | "on" | "and"))
        .map(ScheduleWeekday::parse)
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    weekdays.sort_unstable();
    weekdays.dedup();
    if !weekdays.is_empty() {
        return Ok(AutomationSchedule::Weekly {
            weekdays,
            minute_of_day,
        });
    }

    Err(schedule_help())
}

fn parse_interval(value: &str) -> Result<Option<AutomationSchedule>, ScheduleParseError> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    let (amount, unit) = match tokens.as_slice() {
        ["hourly"] | ["every", "hour" | "hourly"] => (1_u32, "hours"),
        ["every" | "each", amount, unit] => {
            let Ok(amount) = amount.parse::<u32>() else {
                return Ok(None);
            };
            (amount, *unit)
        }
        _ => return Ok(None),
    };
    let minutes = match unit.trim_end_matches('s') {
        "minute" => amount,
        "hour" => amount.checked_mul(60).ok_or_else(schedule_help)?,
        _ => return Ok(None),
    };
    if minutes == 0 || minutes % 5 != 0 {
        return Err(ScheduleParseError(
            "Intervals must be positive and use 5-minute increments.".to_owned(),
        ));
    }
    Ok(Some(AutomationSchedule::IntervalMinutes { minutes }))
}

fn split_time(value: &str) -> Result<(&str, u16), ScheduleParseError> {
    let (cadence, time) = value.rsplit_once(" at ").unwrap_or((value, "00:00"));
    Ok((cadence.trim(), parse_time(time)?))
}

fn parse_time(value: &str) -> Result<u16, ScheduleParseError> {
    let compact = value.trim().replace('.', "");
    let (clock, meridiem) = compact
        .split_once(' ')
        .map_or((compact.as_str(), None), |(clock, suffix)| {
            (clock, Some(suffix))
        });
    let (hour, minute) = clock
        .split_once(':')
        .map_or((clock, "0"), |(hour, minute)| (hour, minute));
    let mut hour = hour.parse::<u16>().map_err(|_| schedule_help())?;
    let minute = minute.parse::<u16>().map_err(|_| schedule_help())?;
    if let Some(meridiem) = meridiem {
        if hour == 0 || hour > 12 {
            return Err(schedule_help());
        }
        match meridiem {
            "am" if hour == 12 => hour = 0,
            "pm" if hour != 12 => hour += 12,
            "am" | "pm" => {}
            _ => return Err(schedule_help()),
        }
    }
    if hour > 23 || minute > 59 {
        return Err(schedule_help());
    }
    if minute % 5 != 0 {
        return Err(ScheduleParseError(
            "Times must use 5-minute increments, such as 2:00 or 2:05 PM.".to_owned(),
        ));
    }
    Ok(hour * 60 + minute)
}

fn parse_monthly(
    cadence: &str,
    minute_of_day: u16,
) -> Result<Option<AutomationSchedule>, ScheduleParseError> {
    if !cadence.starts_with("monthly") && !cadence.starts_with("every month") {
        return Ok(None);
    }
    let day = cadence
        .split_whitespace()
        .find_map(parse_ordinal)
        .unwrap_or(1);
    let Some(day) = u8::try_from(day).ok().filter(|day| (1..=31).contains(day)) else {
        return Err(ScheduleParseError(
            "Monthly schedules require a day from 1 through 31.".to_owned(),
        ));
    };
    Ok(Some(AutomationSchedule::Monthly { day, minute_of_day }))
}

fn parse_yearly(
    cadence: &str,
    minute_of_day: u16,
) -> Result<Option<AutomationSchedule>, ScheduleParseError> {
    let tokens = cadence
        .split_whitespace()
        .filter(|token| !matches!(*token, "every" | "each" | "yearly" | "on"))
        .collect::<Vec<_>>();
    let Some(month) = tokens.first().and_then(|token| parse_month(token)) else {
        return Ok(None);
    };
    let day = tokens
        .get(1)
        .and_then(|token| parse_ordinal(token))
        .unwrap_or(1);
    let narrowed = u8::try_from(month).ok().zip(u8::try_from(day).ok());
    let (Some((month, day)), true) = (
        narrowed,
        NaiveDate::from_ymd_opt(2024, month, day).is_some(),
    ) else {
        return Err(ScheduleParseError(
            "The yearly schedule contains an invalid month and day.".to_owned(),
        ));
    };
    Ok(Some(AutomationSchedule::Yearly {
        month,
        day,
        minute_of_day,
    }))
}

fn parse_ordinal(value: &str) -> Option<u32> {
    value
        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .trim_end_matches(|character: char| character.is_ascii_alphabetic())
        .parse()
        .ok()
}

fn parse_month(value: &str) -> Option<u32> {
    match value.trim_matches(|character: char| !character.is_ascii_alphabetic()) {
        "january" | "jan" => Some(1),
        "february" | "feb" => Some(2),
        "march" | "mar" => Some(3),
        "april" | "apr" => Some(4),
        "may" => Some(5),
        "june" | "jun" => Some(6),
        "july" | "jul" => Some(7),
        "august" | "aug" => Some(8),
        "september" | "sep" | "sept" => Some(9),
        "october" | "oct" => Some(10),
        "november" | "nov" => Some(11),
        "december" | "dec" => Some(12),
        _ => None,
    }
}

fn next_calendar_day(
    after: DateTime<Utc>,
    limit: i64,
    candidate: impl Fn(NaiveDate) -> Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let start = after.with_timezone(&Local).date_naive();
    (0..=limit).find_map(|offset| {
        let date = start.checked_add_signed(Duration::days(offset))?;
        let occurrence = candidate(date)?;
        (occurrence > after).then_some(occurrence)
    })
}

fn local_occurrence(date: NaiveDate, minute_of_day: u16) -> Option<DateTime<Utc>> {
    let hour = u32::from(minute_of_day / 60);
    let minute = u32::from(minute_of_day % 60);
    match Local.with_ymd_and_hms(date.year(), date.month(), date.day(), hour, minute, 0) {
        LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
            Some(value.with_timezone(&Utc))
        }
        LocalResult::None => {
            let skipped = date.and_hms_opt(hour, minute, 0)?;
            (1..=120).find_map(|offset| {
                match Local
                    .from_local_datetime(&skipped.checked_add_signed(Duration::minutes(offset))?)
                {
                    LocalResult::Single(value) | LocalResult::Ambiguous(value, _) => {
                        Some(value.with_timezone(&Utc))
                    }
                    LocalResult::None => None,
                }
            })
        }
    }
}

fn schedule_help() -> ScheduleParseError {
    ScheduleParseError(
        "Use a schedule like \"Every Wednesday at 2:00 PM\", \"Every 30 minutes\", \
         \"Weekdays at 9:00\", \"Monthly on the 1st at 8:00\", or \
         \"Every January 15 at 10:00\"."
            .to_owned(),
    )
}

/// The next time a schedule should fire, given when it last fired.
///
/// Interval schedules stay anchored to `previous` so a late dispatch does not
/// drift the cadence, and missed ticks collapse into the next future slot
/// rather than firing a backlog. Calendar schedules walk forward from the
/// previous occurrence until they pass `now`.
///
/// Returns `None` when `previous` is not a valid RFC 3339 timestamp or the
/// schedule has no representable next occurrence.
#[must_use]
pub fn next_automation_occurrence(
    schedule: &AutomationSchedule,
    previous: &str,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let previous = DateTime::parse_from_rfc3339(previous)
        .ok()?
        .with_timezone(&Utc);
    if let AutomationSchedule::IntervalMinutes { minutes } = schedule {
        let interval = i64::from(*minutes);
        let elapsed_minutes = (now - previous).num_minutes().max(0);
        let steps = elapsed_minutes / interval + 1;
        return previous.checked_add_signed(Duration::minutes(interval.checked_mul(steps)?));
    }
    let mut next = schedule.next_after(previous)?;
    for _ in 0..10_000 {
        if next > now {
            return Some(next);
        }
        next = schedule.next_after(next)?;
    }
    None
}

/// Interpret an automation condition's final answer.
///
/// The condition prompt demands a single word, but models wrap it in
/// backticks or punctuation often enough that tolerating that is worth more
/// than being strict. Anything else is an error rather than a guess, so an
/// ambiguous answer never silently decides whether the action runs.
///
/// # Errors
///
/// Returns an error when the response does not reduce to `true` or `false`.
pub fn parse_automation_condition(response: &str) -> Result<bool, String> {
    let normalized = response
        .trim_matches(|character: char| {
            character.is_whitespace() || matches!(character, '`' | '.' | '!')
        })
        .to_ascii_lowercase();
    match normalized.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!(
            "Automation condition did not return true or false: {}",
            response.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn parses_interval_and_rejects_sub_five_minute_granularity() {
        assert_eq!(
            "every 30 minutes".parse(),
            Ok(AutomationSchedule::IntervalMinutes { minutes: 30 })
        );
        assert_eq!(
            "every 2 hours".parse(),
            Ok(AutomationSchedule::IntervalMinutes { minutes: 120 })
        );
        assert!("every 3 minutes".parse::<AutomationSchedule>().is_err());
    }

    #[test]
    fn parses_weekdays_and_named_days() {
        assert_eq!(
            "weekdays at 9:05 AM".parse(),
            Ok(AutomationSchedule::Weekly {
                weekdays: vec![
                    ScheduleWeekday::Monday,
                    ScheduleWeekday::Tuesday,
                    ScheduleWeekday::Wednesday,
                    ScheduleWeekday::Thursday,
                    ScheduleWeekday::Friday,
                ],
                minute_of_day: 9 * 60 + 5,
            })
        );
        assert_eq!(
            "every Monday and Friday at 11:55 PM".parse(),
            Ok(AutomationSchedule::Weekly {
                weekdays: vec![ScheduleWeekday::Monday, ScheduleWeekday::Friday],
                minute_of_day: 23 * 60 + 55,
            })
        );
        assert_eq!(
            "weekends at 12:00 AM".parse(),
            Ok(AutomationSchedule::Weekly {
                weekdays: vec![ScheduleWeekday::Saturday, ScheduleWeekday::Sunday],
                minute_of_day: 0,
            })
        );
        assert_eq!(
            "Every Wednesday at 2:00".parse(),
            Ok(AutomationSchedule::Weekly {
                weekdays: vec![ScheduleWeekday::Wednesday],
                minute_of_day: 2 * 60,
            })
        );
    }

    #[test]
    fn parses_monthly_and_yearly_schedules() {
        assert_eq!(
            "monthly on the 15th at 08:30".parse(),
            Ok(AutomationSchedule::Monthly {
                day: 15,
                minute_of_day: 8 * 60 + 30,
            })
        );
        assert_eq!(
            "every January 15 at 10:00".parse(),
            Ok(AutomationSchedule::Yearly {
                month: 1,
                day: 15,
                minute_of_day: 10 * 60,
            })
        );
    }

    #[test]
    fn interval_next_run_is_on_five_minute_boundary() {
        let after = Utc.with_ymd_and_hms(2026, 8, 14, 20, 37, 42).unwrap();
        let next = AutomationSchedule::IntervalMinutes { minutes: 30 }
            .next_after(after)
            .unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 8, 14, 21, 10, 0).unwrap());
    }

    #[test]
    fn monthly_schedules_clamp_to_the_last_day() {
        let after = Utc.with_ymd_and_hms(2027, 1, 31, 23, 0, 0).unwrap();
        let next = AutomationSchedule::Monthly {
            day: 31,
            minute_of_day: 9 * 60,
        }
        .next_after(after)
        .unwrap();
        let local = next.with_timezone(&Local);
        assert_eq!((local.year(), local.month(), local.day()), (2027, 2, 28));
    }

    #[test]
    fn monthly_schedules_reject_day_zero() {
        assert!(
            "monthly on the 0th at 9:00"
                .parse::<AutomationSchedule>()
                .is_err()
        );
    }

    #[test]
    fn parses_daily_aliases_and_twelve_hour_edges() {
        assert_eq!(
            "every day at 12:00 AM".parse(),
            Ok(AutomationSchedule::Daily { minute_of_day: 0 })
        );
        assert_eq!(
            "daily at 12:00 PM".parse(),
            Ok(AutomationSchedule::Daily {
                minute_of_day: 12 * 60,
            })
        );
        assert_eq!(
            "each day at 7:45 pm".parse(),
            Ok(AutomationSchedule::Daily {
                minute_of_day: 19 * 60 + 45,
            })
        );
    }

    #[test]
    fn rejects_invalid_time_and_calendar_inputs() {
        for invalid in [
            "",
            "every 0 minutes",
            "every 7 minutes",
            "daily at 24:00",
            "daily at 9:03",
            "daily at 13:00 PM",
            "monthly on the 32nd at 9:00",
            "every February 30 at 10:00",
            "whenever convenient",
        ] {
            assert!(
                invalid.parse::<AutomationSchedule>().is_err(),
                "{invalid:?} unexpectedly parsed"
            );
        }
    }

    #[test]
    fn calendar_next_runs_are_strictly_future_and_match_local_fields() {
        let after = Utc.with_ymd_and_hms(2026, 8, 14, 20, 37, 42).unwrap();
        for (schedule, expected_hour, expected_minute) in [
            (
                AutomationSchedule::Daily {
                    minute_of_day: 9 * 60 + 5,
                },
                9,
                5,
            ),
            (
                AutomationSchedule::Weekly {
                    weekdays: vec![ScheduleWeekday::Monday],
                    minute_of_day: 14 * 60 + 10,
                },
                14,
                10,
            ),
            (
                AutomationSchedule::Monthly {
                    day: 15,
                    minute_of_day: 8 * 60 + 30,
                },
                8,
                30,
            ),
            (
                AutomationSchedule::Yearly {
                    month: 1,
                    day: 15,
                    minute_of_day: 10 * 60,
                },
                10,
                0,
            ),
        ] {
            let next = schedule.next_after(after).expect("schedule has a next run");
            let local = next.with_timezone(&Local);
            assert!(next > after);
            assert_eq!(local.minute(), expected_minute);
            assert_eq!(local.hour(), expected_hour);
            if let AutomationSchedule::Weekly { weekdays, .. } = schedule {
                assert!(
                    weekdays
                        .iter()
                        .any(|weekday| weekday.chrono() == local.weekday())
                );
            }
        }
    }

    #[test]
    fn automation_status_strings_round_trip() {
        for status in [
            AutomationRunStatus::Running,
            AutomationRunStatus::Skipped,
            AutomationRunStatus::Succeeded,
            AutomationRunStatus::Failed,
        ] {
            assert_eq!(
                AutomationRunStatus::from_str_or_running(status.as_str()),
                status
            );
        }
        assert_eq!(
            AutomationRunStatus::from_str_or_running("unknown"),
            AutomationRunStatus::Running
        );
    }

    #[test]
    fn automation_condition_requires_an_unambiguous_boolean() {
        assert_eq!(parse_automation_condition("true"), Ok(true));
        assert_eq!(parse_automation_condition("`false`."), Ok(false));
        assert!(parse_automation_condition("It looks true").is_err());
    }

    #[test]
    fn interval_automation_stays_anchored_when_dispatch_is_late() {
        let now = DateTime::parse_from_rfc3339("2026-08-14T10:10:07Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_automation_occurrence(
            &AutomationSchedule::IntervalMinutes { minutes: 5 },
            "2026-08-14T10:10:00Z",
            now,
        )
        .unwrap();
        assert_eq!(next.to_rfc3339(), "2026-08-14T10:15:00+00:00");
    }

    #[test]
    fn interval_automation_skips_missed_ticks_without_drifting() {
        let now = DateTime::parse_from_rfc3339("2026-08-14T10:27:31Z")
            .unwrap()
            .with_timezone(&Utc);
        let next = next_automation_occurrence(
            &AutomationSchedule::IntervalMinutes { minutes: 5 },
            "2026-08-14T10:10:00Z",
            now,
        )
        .unwrap();
        assert_eq!(next.to_rfc3339(), "2026-08-14T10:30:00+00:00");
    }
}
