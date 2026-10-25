//! 公历日期换算：Unix 时间到年月日、星期与月历；不涉及时区，调用者决定显示 UTC 还是本地时间。

/// 公历日期。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Date {
    pub year: i64,
    /// 1..=12。
    pub month: u32,
    /// 1..=31。
    pub day: u32,
}

/// 自 1970-01-01 起的天数换成公历日期（Howard Hinnant 的 civil_from_days，负数天数也成立）。
pub fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    Date {
        year: year_of_era + era * 400 + i64::from(month <= 2),
        month,
        day,
    }
}

/// 公历日期换成自 1970-01-01 起的天数；`civil_from_days` 的逆运算。
pub fn days_from_civil(date: Date) -> i64 {
    let year = date.year - i64::from(date.month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = i64::from(if date.month > 2 {
        date.month - 3
    } else {
        date.month + 9
    });
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(date.day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// 星期：0 为星期日，6 为星期六（1970-01-01 是星期四）。
pub fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

pub fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 某月的月历：每行七天、从星期日开始；月外的格子是 `None`。
pub fn month_grid(year: i64, month: u32) -> Vec<[Option<u32>; 7]> {
    let first = weekday(days_from_civil(Date {
        year,
        month,
        day: 1,
    })) as usize;
    let mut rows = Vec::new();
    let mut row = [None; 7];
    for day in 1..=days_in_month(year, month) {
        row[(first + day as usize - 1) % 7] = Some(day);
        if (first + day as usize).is_multiple_of(7) {
            rows.push(std::mem::take(&mut row));
        }
    }
    if row.iter().any(Option::is_some) {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_dates_round_trip_across_eras_and_the_epoch() {
        assert_eq!(
            civil_from_days(0),
            Date {
                year: 1970,
                month: 1,
                day: 1
            }
        );
        for days in [-800_000, -1, 0, 11_016, 951_782_400 / 86_400, 2_932_896] {
            assert_eq!(days_from_civil(civil_from_days(days)), days);
        }
        assert_eq!(
            civil_from_days(951_782_400 / 86_400),
            Date {
                year: 2000,
                month: 2,
                day: 29
            }
        );
    }

    #[test]
    fn weekdays_match_known_dates() {
        assert_eq!(weekday(0), 4, "1970-01-01 was a Thursday");
        let y2k = days_from_civil(Date {
            year: 2000,
            month: 1,
            day: 1,
        });
        assert_eq!(weekday(y2k), 6, "2000-01-01 was a Saturday");
        assert_eq!(weekday(-1), 3);
    }

    #[test]
    fn month_lengths_follow_the_leap_rules() {
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 4), 30);
        assert_eq!(days_in_month(2023, 12), 31);
    }

    #[test]
    fn the_month_grid_places_every_day_on_the_right_weekday() {
        // 2000 年 2 月：星期二开始，29 天，共 5 行。
        let grid = month_grid(2000, 2);
        assert_eq!(grid.len(), 5);
        assert_eq!(
            grid[0],
            [None, None, Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
        assert_eq!(grid[4][2], Some(29));
        assert_eq!(grid.iter().flatten().flatten().count(), 29);
        // 恰好从星期日开始、28 天的月份占满四行（2015 年 2 月）。
        assert_eq!(month_grid(2015, 2).len(), 4);
    }
}
