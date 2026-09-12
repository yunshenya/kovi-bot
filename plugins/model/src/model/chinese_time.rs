//! 中文时间表达解析：把"明天下午三点""下周三晚上""月底"这类说法算成具体时刻。
//!
//! 为什么需要它：现在这件事**整个交给模型**——工具说明里写着"先用 `time.now` 获取当前
//! 日期，不要猜测日期"，然后由模型自己做"今天是周五 + 下周三 = 9 月 16 日"这类推算，
//! 再填成 `local_datetime`。模型做日期算术是出了名的不可靠（跨月、跨年、闰年、"这周三"
//! 到底指哪一周），而算错一天的代价是**她在错的日子叫你**。
//!
//! Hindsight 为此专门写了 `chinese_temporal_periods.py`（86KB）。这里是够用的那一半：
//! 确定性的词法解析 + 明确的精度标记，算不出来就如实说算不出来，让上层去问。
//!
//! 两条原则：
//! - **不猜**：解析不出来就返回 `None`，绝不返回一个"大概吧"的时刻；
//! - **说清精度**：只说了"明天下午"（没给钟点）时，返回的是按惯例取的 15:00，
//!   同时标记 [`TimePrecision::Period`]，让上层能说"我按下午三点记的"而不是假装精确。

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;

/// 解析结果的精度。上层据此决定要不要跟对方确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimePrecision {
    /// 说到了分钟（"三点半""8:20"）。
    Minute,
    /// 只说了时间段（"明天下午"），钟点是按惯例取的。
    Period,
    /// 只说了日期（"下周三"），没有时间信息。
    Date,
}

/// 一次解析的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTime {
    /// 解析出的时刻（带时区）。
    pub at: DateTime<Tz>,
    pub precision: TimePrecision,
    /// 被理解的那段原文，便于回显确认。
    pub matched: String,
    /// 该时刻已经过去了。上层应当据此追问，而不是照着建一个过去的提醒。
    pub in_past: bool,
}

/// 时段按惯例对应的钟点。没有钟点时用它，并标记 [`TimePrecision::Period`]。
fn period_default_hour(period: &str) -> Option<u32> {
    Some(match period {
        "凌晨" => 3,
        "早上" | "早晨" | "一早" => 8,
        "上午" => 10,
        "中午" | "正午" => 12,
        "下午" => 15,
        "傍晚" | "黄昏" => 18,
        "晚上" | "晚间" | "今晚" => 20,
        "夜里" | "半夜" | "深夜" | "今夜" => 22,
        "今早" | "今晨" => 8,
        _ => return None,
    })
}

const PERIODS: &[&str] = &[
    "凌晨", "早上", "早晨", "一早", "上午", "中午", "正午", "下午", "傍晚", "黄昏", "晚上", "晚间",
    "夜里", "半夜", "深夜", "今晚", "今夜", "今早", "今晨",
];

/// 这段时间属于"下午之后"（12 小时制要 +12）。
fn is_afternoon_period(period: &str) -> bool {
    [
        "下午", "傍晚", "黄昏", "晚上", "晚间", "夜里", "半夜", "深夜", "今晚", "今夜",
    ]
    .iter()
    .any(|candidate| candidate == &period)
}

/// 这段时间属于"中午之前"（12 点要归零）。
fn is_morning_period(period: &str) -> bool {
    ["凌晨", "早上", "早晨", "一早", "上午", "今早", "今晨"]
        .iter()
        .any(|candidate| candidate == &period)
}

/// 中文数字（含"两"）→ 数值。只处理 0..=99，够用于钟点和天数。
pub(crate) fn chinese_number(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = text.parse::<u32>() {
        return Some(value);
    }
    let digits = |character: char| -> Option<u32> {
        Some(match character {
            '零' | '〇' => 0,
            '一' | '壹' => 1,
            '二' | '两' | '贰' => 2,
            '三' | '叁' => 3,
            '四' | '肆' => 4,
            '五' | '伍' => 5,
            '六' | '陆' => 6,
            '七' | '柒' => 7,
            '八' | '捌' => 8,
            '九' | '玖' => 9,
            _ => return None,
        })
    };
    let characters: Vec<char> = text.chars().collect();
    match characters.len() {
        1 if characters[0] == '十' => Some(10),
        1 => digits(characters[0]),
        2 if characters[0] == '十' => Some(10 + digits(characters[1]).unwrap_or(0)),
        2 if characters[1] == '十' => Some(digits(characters[0])? * 10),
        3 if characters[1] == '十' => {
            Some(digits(characters[0])? * 10 + digits(characters[2]).unwrap_or(0))
        }
        _ => None,
    }
}

/// 主入口：在 `text` 里找中文时间表达，算成 `now` 所在时区里的具体时刻。
///
/// 找不到就返回 `None`——**不猜**。
pub(crate) fn resolve(text: &str, now: DateTime<Tz>) -> Option<ResolvedTime> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // ① 纯相对量（"三天后""两个小时后""半小时后"）——这类是完整时刻，优先级最高。
    if let Some(resolved) = resolve_offset(text, now) {
        return Some(resolved);
    }

    // ② 基日 → ③ 时段 → ④ 钟点，三段各自独立匹配再组合。
    let (date, date_matched) = resolve_base_date(text, now)?;
    let period = PERIODS
        .iter()
        .find(|period| text.contains(**period))
        .copied();
    let clock = resolve_clock(text);

    let (hour, minute, precision) = match (clock, period) {
        (Some((hour, minute)), _) => (hour, minute, TimePrecision::Minute),
        (None, Some(period)) => (period_default_hour(period)?, 0, TimePrecision::Period),
        (None, None) => (0, 0, TimePrecision::Date),
    };

    // "晚上12点"在中文里指**次日零点**，不是中午十二点。跨日必须显式处理，
    // 否则会给出一个差了 12 小时、还差一天的时刻。
    let afternoon_midnight = period.is_some_and(is_afternoon_period) && hour == 12;
    let day_shift = if afternoon_midnight { 1_i64 } else { 0 };
    let hour = if afternoon_midnight { 0 } else { hour };
    let date = date + Duration::days(day_shift);

    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let naive = NaiveDateTime::new(date, time);
    let at = now.timezone().from_local_datetime(&naive).single()?;

    let mut matched = date_matched;
    if let Some(period) = period {
        matched.push_str(period);
    }
    if let Some((hour, minute)) = clock {
        matched.push_str(&format!("{hour:02}:{minute:02}"));
    }

    Some(ResolvedTime {
        at,
        precision,
        matched,
        in_past: at <= now,
    })
}

/// "三天后""两小时后""半小时后""一周后"——相对当前时刻的偏移。
fn resolve_offset(text: &str, now: DateTime<Tz>) -> Option<ResolvedTime> {
    let number_before = |unit: &str| -> Option<u32> {
        let index = text.find(unit)?;
        // 跳过量词："三个小时后" 的数字在"个"之前。
        let head = text[..index].trim_end_matches(['个', '整', '多', '来']);
        // 取紧邻单位之前的数字（阿拉伯或中文）。
        let digits: String = head
            .chars()
            .rev()
            .take_while(|character| {
                character.is_ascii_digit() || "零〇一二两三四五六七八九十".contains(*character)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        chinese_number(&digits)
    };
    let unit_seconds: &[(&str, i64)] = &[
        ("秒钟", 1),
        ("秒", 1),
        ("分钟", 60),
        ("分", 60),
        ("个钟头", 3_600),
        ("小时", 3_600),
        ("钟头", 3_600),
        ("天", 86_400),
        ("周", 604_800),
        ("星期", 604_800),
        ("个月", 2_592_000),
    ];
    // 特殊说法先处理。
    // 只有"半小时后"这种明确的数量才解析。"一会儿""待会儿"是模糊说法，
    // 按本模块的第一条原则**不猜**——返回 None，让上层去问。
    for (phrase, seconds) in [("半小时后", 1_800_i64)] {
        if text.contains(phrase) {
            return Some(ResolvedTime {
                at: now + Duration::seconds(seconds),
                precision: TimePrecision::Minute,
                matched: phrase.to_string(),
                in_past: false,
            });
        }
    }
    for (unit, seconds) in unit_seconds {
        let Some(index) = text.find(unit) else {
            continue;
        };
        // 必须是"数量 + 单位 + 后/之后/以后"，否则不是相对时间（"三天前"是过去）。
        let tail = &text[index + unit.len()..];
        let forward =
            tail.starts_with('后') || tail.starts_with("之后") || tail.starts_with("以后");
        if !forward {
            continue;
        }
        let Some(count) = number_before(unit) else {
            continue;
        };
        let at = now + Duration::seconds(seconds.saturating_mul(i64::from(count)));
        return Some(ResolvedTime {
            at,
            precision: TimePrecision::Minute,
            matched: format!("{count}{unit}后"),
            in_past: false,
        });
    }
    None
}

/// 基日：今天/明天/后天…、周X、X月X号、月初/月底、今年/明年。
fn resolve_base_date(text: &str, now: DateTime<Tz>) -> Option<(NaiveDate, String)> {
    let today = now.date_naive();

    // 明确的年月日："2026年9月16日" / "9月16号" / "9月16日"
    if let Some((date, matched)) = resolve_explicit_date(text, now) {
        return Some((date, matched));
    }

    // 月边界："这个月月底" "下月初" "月底"
    if let Some((date, matched)) = resolve_month_boundary(text, today) {
        return Some((date, matched));
    }

    // 周："下周三" "这周天" "周三" "周末"
    if let Some((date, matched)) = resolve_weekday(text, today) {
        return Some((date, matched));
    }

    // 相对日：大后天要排在"后天"前面匹配。
    for (phrase, offset) in [
        ("大后天", 3_i64),
        ("后天", 2),
        ("明天", 1),
        ("明日", 1),
        ("今天", 0),
        ("今日", 0),
        // "今晚八点""今早"里的"今"也是今天，但下面的日期表按整词匹配，
        // 所以单列出来。
        ("今晚", 0),
        ("今早", 0),
        ("今晨", 0),
        ("今夜", 0),
        ("昨天", -1),
        ("昨日", -1),
        ("前天", -2),
    ] {
        if text.contains(phrase) {
            return Some((today + Duration::days(offset), phrase.to_string()));
        }
    }
    None
}

fn resolve_explicit_date(text: &str, now: DateTime<Tz>) -> Option<(NaiveDate, String)> {
    let today = now.date_naive();
    // 2026年9月16日 / 9月16号 / 9月16日
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] != '月' {
            index += 1;
            continue;
        }
        // 往前取月份数字，往后取日期数字。
        let month_digits: String = characters[..index]
            .iter()
            .rev()
            .take_while(|character| character.is_ascii_digit() || is_chinese_digit(**character))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let tail: String = characters[index + 1..].iter().collect();
        let day_index = tail.find(['日', '号']);
        if let (Some(month), Some(day_index)) = (chinese_number(&month_digits), day_index) {
            let day_digits: String = tail[..day_index]
                .chars()
                .take_while(|character| character.is_ascii_digit() || is_chinese_digit(*character))
                .collect();
            if let Some(day) = chinese_number(&day_digits) {
                let year = resolve_year(text, today.year());
                if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                    return Some((date, format!("{year}年{month}月{day}日")));
                }
            }
        }
        index += 1;
    }
    None
}

fn resolve_year(text: &str, current_year: i32) -> i32 {
    if text.contains("明年") || text.contains("下一年") {
        current_year + 1
    } else if text.contains("去年") || text.contains("上一年") {
        current_year - 1
    } else {
        current_year
    }
}

fn resolve_month_boundary(text: &str, today: NaiveDate) -> Option<(NaiveDate, String)> {
    let (month_offset, label) = if text.contains("下个月") || text.contains("下月") {
        (1_i32, "下个月")
    } else if text.contains("上个月") || text.contains("上月") {
        (-1, "上个月")
    } else if text.contains("这个月") || text.contains("本月") {
        (0, "这个月")
    } else if text.contains("月底") || text.contains("月初") {
        (0, "")
    } else {
        return None;
    };
    let (year, month) = shift_month(today.year(), today.month(), month_offset)?;
    let last_day = days_in_month(year, month);
    if text.contains("月底") || text.contains("月末") {
        let date = NaiveDate::from_ymd_opt(year, month, last_day)?;
        return Some((date, format!("{label}月底")));
    }
    if text.contains("月初") {
        let date = NaiveDate::from_ymd_opt(year, month, 1)?;
        return Some((date, format!("{label}月初")));
    }
    // 只说了"下个月"：按下月同日，超出月末就取月末。
    let day = today.day().min(last_day);
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some((date, label.to_string()))
}

fn shift_month(year: i32, month: u32, offset: i32) -> Option<(i32, u32)> {
    let total = year * 12 + (month as i32 - 1) + offset;
    let shifted_year = total.div_euclid(12);
    let shifted_month = total.rem_euclid(12) + 1;
    Some((shifted_year, shifted_month as u32))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_next = NaiveDate::from_ymd_opt(next_year, next_month, 1).expect("下月一日总是合法");
    (first_next - Duration::days(1)).day()
}

fn resolve_weekday(text: &str, today: NaiveDate) -> Option<(NaiveDate, String)> {
    let weekdays = [
        ("一", 0_u32),
        ("二", 1),
        ("三", 2),
        ("四", 3),
        ("五", 4),
        ("六", 5),
        ("日", 6),
        ("天", 6),
    ];
    let weekday_index = |character: &str| weekdays.iter().find(|(name, _)| *name == character);
    let today_index = today.weekday().num_days_from_monday();

    // 周末 / 下周末 / 这周末：按周六算。
    if text.contains("周末") || text.contains("週末") {
        let (week_offset, label) = if text.contains("下周末") {
            (1_i64, "下周末")
        } else if text.contains("这周末") || text.contains("本周末") {
            (0, "这周末")
        } else {
            (0, "周末")
        };
        let days_to_saturday = i64::from(5_u32.saturating_sub(today_index));
        let date = today + Duration::days(days_to_saturday + week_offset * 7);
        return Some((date, label.to_string()));
    }

    for marker in [
        "下周", "下週", "本周", "这周", "这週", "上周", "上週", "周", "週", "星期", "礼拜",
    ] {
        let Some(index) = text.find(marker) else {
            continue;
        };
        let tail = &text[index + marker.len()..];
        let Some(character) = tail.chars().next() else {
            continue;
        };
        let Some((name, target_index)) = weekday_index(&character.to_string()) else {
            continue;
        };
        let week_offset: i64 = match marker {
            "下周" | "下週" => 1,
            "上周" | "上週" => -1,
            _ => 0,
        };
        let days = i64::from(*target_index) - i64::from(today_index) + week_offset * 7;
        let date = today + Duration::days(days);
        let _ = name;
        return Some((date, format!("{marker}{character}")));
    }
    None
}

/// 按上下文里的时段把 12 小时制换算成 24 小时制。
///
/// 两种写法（"下午三点"与"下午3:20"）都要走这一关——冒号形式曾经提前返回，
/// 漏了换算，"下午3:20"就成了凌晨 3:20。
fn apply_period_offset(hour: u32, text: &str) -> u32 {
    let Some(period) = PERIODS.iter().find(|period| text.contains(**period)) else {
        return hour;
    };
    let mut hour = hour;
    if is_afternoon_period(period) && hour < 12 {
        hour += 12;
    }
    if is_morning_period(period) && hour == 12 {
        hour = 0;
    }
    hour
}

/// 钟点："三点半""15:20""晚上8点20"里的具体时刻。
fn resolve_clock(text: &str) -> Option<(u32, u32)> {
    // 15:20 / 15：20
    for separator in [':', '：'] {
        if let Some(index) = text.find(separator) {
            let head: String = text[..index]
                .chars()
                .rev()
                .take_while(|character| character.is_ascii_digit())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let tail: String = text[index + separator.len_utf8()..]
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect();
            if let (Some(hour), Some(minute)) = (chinese_number(&head), chinese_number(&tail))
                && hour <= 23
                && minute <= 59
            {
                return Some((apply_period_offset(hour, text), minute));
            }
        }
    }

    let index = text.find('点')?;
    let head: String = text[..index]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit() || is_chinese_digit(*character))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut hour = chinese_number(&head)?;
    if hour > 23 {
        return None;
    }

    let tail: String = text[index + '点'.len_utf8()..].chars().take(4).collect();
    let minute = if tail.starts_with('半') {
        30
    } else if tail.starts_with("一刻") {
        15
    } else if tail.starts_with("三刻") {
        45
    } else {
        let minute_digits: String = tail
            .chars()
            .take_while(|character| character.is_ascii_digit() || is_chinese_digit(*character))
            .collect();
        if minute_digits.is_empty() {
            0
        } else {
            chinese_number(&minute_digits)?
        }
    };
    if minute > 59 {
        return None;
    }

    // 下午/晚上 的 12 小时制换算：下午三点 = 15 点。
    hour = apply_period_offset(hour, text);
    Some((hour, minute))
}

fn is_chinese_digit(character: char) -> bool {
    "零〇一二两三四五六七八九十".contains(character)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 固定"现在"：2026-09-11（周五）14:30，Asia/Shanghai。
    /// 周五这个位置很关键：往前有周三（已过）、往后有周末与下周一。
    fn now() -> DateTime<Tz> {
        chrono_tz::Asia::Shanghai
            .with_ymd_and_hms(2026, 9, 11, 14, 30, 0)
            .single()
            .expect("固定测试时刻")
    }

    fn at(text: &str) -> String {
        resolve(text, now())
            .unwrap_or_else(|| panic!("应能解析：{text}"))
            .at
            .format("%Y-%m-%d %H:%M")
            .to_string()
    }

    #[test]
    fn relative_days_resolve_against_today() {
        assert_eq!(at("今天"), "2026-09-11 00:00");
        assert_eq!(at("明天"), "2026-09-12 00:00");
        assert_eq!(at("后天"), "2026-09-13 00:00");
        assert_eq!(at("大后天"), "2026-09-14 00:00");
        assert_eq!(at("昨天"), "2026-09-10 00:00");
    }

    #[test]
    fn weekday_expressions_land_on_the_right_week() {
        // 周五看到的"下周三"是下下周？不——下周从 9/14 起，所以是 9/16。
        assert_eq!(at("下周三"), "2026-09-16 00:00");
        // "这周三"已经过去（9/9），如实返回过去的时间，由上层决定追问。
        assert_eq!(at("这周三"), "2026-09-09 00:00");
        assert!(resolve("这周三", now()).expect("可解析").in_past);
        // 不写周次的"周三"按本周算。
        assert_eq!(at("周三"), "2026-09-09 00:00");
        assert_eq!(at("周六"), "2026-09-12 00:00");
        assert_eq!(at("周天"), "2026-09-13 00:00");
        assert_eq!(at("周末"), "2026-09-12 00:00");
        assert_eq!(at("下周末"), "2026-09-19 00:00");
    }

    #[test]
    fn clock_times_understand_half_quarter_and_periods() {
        assert_eq!(at("明天下午三点半"), "2026-09-12 15:30");
        assert_eq!(at("明天早上八点"), "2026-09-12 08:00");
        assert_eq!(at("今晚八点"), "2026-09-11 20:00");
        assert_eq!(at("明天下午3:20"), "2026-09-12 15:20");
        assert_eq!(at("明天九点一刻"), "2026-09-12 09:15");
        assert_eq!(at("明天九点三刻"), "2026-09-12 09:45");
        // "晚上12点"在中文里指次日零点——既不是中午 12 点，也不是当天。
        assert_eq!(at("明天晚上12点"), "2026-09-13 00:00");
        // 凌晨不该被加成下午。
        assert_eq!(at("明天凌晨两点"), "2026-09-12 02:00");
    }

    #[test]
    fn period_only_keeps_its_precision_marked() {
        let resolved = resolve("明天下午", now()).expect("应能解析");
        assert_eq!(
            resolved.at.format("%Y-%m-%d %H:%M").to_string(),
            "2026-09-12 15:00"
        );
        // 只说"下午"没给钟点：必须标成 Period，上层才敢说"我按三点记的"。
        assert_eq!(resolved.precision, TimePrecision::Period);
        let dated = resolve("下周三", now()).expect("应能解析");
        assert_eq!(dated.precision, TimePrecision::Date);
        let minute = resolve("明天下午三点半", now()).expect("应能解析");
        assert_eq!(minute.precision, TimePrecision::Minute);
    }

    #[test]
    fn month_boundaries_handle_short_months() {
        assert_eq!(at("这个月月底"), "2026-09-30 00:00");
        assert_eq!(at("下个月月底"), "2026-10-31 00:00");
        assert_eq!(at("下个月初"), "2026-10-01 00:00");
        // 月底/月初不带月份时按本月算。
        assert_eq!(at("月底"), "2026-09-30 00:00");
        // 31 号说"下个月"：十月只有 31 天，仍是 31；二月要收窄。
        let february = resolve(
            "下个月",
            chrono_tz::Asia::Shanghai
                .with_ymd_and_hms(2027, 1, 31, 9, 0, 0)
                .single()
                .expect("固定时刻"),
        )
        .expect("应能解析");
        assert_eq!(february.at.format("%Y-%m-%d").to_string(), "2027-02-28");
    }

    #[test]
    fn offsets_are_relative_to_now_not_to_midnight() {
        let resolved = resolve("三个小时后", now()).expect("应能解析");
        assert_eq!(
            resolved.at.format("%Y-%m-%d %H:%M").to_string(),
            "2026-09-11 17:30"
        );
        assert_eq!(resolved.precision, TimePrecision::Minute);
        assert_eq!(at("半小时后"), "2026-09-11 15:00");
        assert_eq!(at("两天后"), "2026-09-13 14:30");
        // "三天前"不是未来，不该被当成相对未来解析。
        assert!(resolve("三天前", now()).is_none());
    }

    #[test]
    fn explicit_dates_and_years_are_honoured() {
        assert_eq!(at("9月16号"), "2026-09-16 00:00");
        assert_eq!(at("2026年9月16日"), "2026-09-16 00:00");
        assert_eq!(at("明年1月3号"), "2027-01-03 00:00");
    }

    #[test]
    fn unparsable_input_returns_none_instead_of_guessing() {
        // 不猜：算不出来就交给上层去问，而不是返回一个"大概吧"的时刻。
        for text in ["随便吧", "有空的时候", "一会儿再说吧", "看你"] {
            assert!(resolve(text, now()).is_none(), "{text} 不该被解析出时刻");
        }
        assert!(resolve("   ", now()).is_none());
    }

    #[test]
    fn chinese_numbers_cover_the_ranges_we_use() {
        assert_eq!(chinese_number("三"), Some(3));
        assert_eq!(chinese_number("两"), Some(2));
        assert_eq!(chinese_number("十"), Some(10));
        assert_eq!(chinese_number("十五"), Some(15));
        assert_eq!(chinese_number("二十"), Some(20));
        assert_eq!(chinese_number("二十三"), Some(23));
        assert_eq!(chinese_number("三十"), Some(30));
        assert_eq!(chinese_number("59"), Some(59));
        assert_eq!(chinese_number("没有数字"), None);
    }
}
