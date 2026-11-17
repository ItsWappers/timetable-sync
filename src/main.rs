use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate, Timelike};
use clap::Parser;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;

/// WebUntis timetable scraper: login, fetch week/day, save JSON, answer queries.
///
/// No args: current week (Sat/Sun -> next week) into timetables/timetable.json.
/// Date args take DD/MM/YYYY (also DD.MM.YYYY, YYYY-MM-DD).
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Class timetable, case-insensitive (e.g. 4ag). Default: personal.
    #[arg(short = 'c', long = "class", alias = "timetable", short_alias = 'T')]
    class: Option<String>,

    /// Disable class filtering (include everything).
    #[arg(long = "all")]
    all: bool,

    /// Only today.
    #[arg(long = "today")]
    today: bool,

    /// Whole week Mon-Sun. Bare: current week. With date: week containing it.
    #[arg(short = 'w', long = "week", num_args(0..=1), default_missing_value = "__current__")]
    week: Option<String>,

    /// Next school day (Sat/Sun/Fri-pm roll to Monday).
    #[arg(short = 't', long = "tomorrow", alias = "next")]
    tomorrow: bool,

    /// Single date DD/MM/YYYY. File: timetables/<date>-<who>.json
    #[arg(short = 'd', long = "date")]
    date: Option<String>,

    /// Weekday in queried week (mon..sun, also wednesday etc).
    #[arg(long = "day")]
    day: Option<String>,

    /// Nth lesson of the day (1-indexed, by start time).
    #[arg(long = "period", short_alias = 'p')]
    period: Option<usize>,

    /// Only cancelled / substituted lessons, today + next school day.
    /// For cron/Hermes: prints alerts, nothing when all regular.
    #[arg(long = "alerts")]
    alerts: bool,

    /// List upcoming tests/Schularbeiten from tests.json (Hermes reminders).
    #[arg(long = "tests")]
    tests: bool,

    /// Window in days for --tests (default 14).
    #[arg(long = "days", default_value_t = 14)]
    days: i64,

    /// Week offset: +1 next week, -1 last week.
    #[arg(long = "week-offset", default_value_t = 0, allow_hyphen_values = true)]
    week_offset: i64,

    /// Output file (bare name lands in timetables/).
    #[arg(long = "out", default_value = "timetable.json")]
    out: String,

    /// Back-compat positional output file.
    #[arg(index = 1)]
    positional_out: Option<String>,

    /// Output to stdout: json | text.
    #[arg(long = "format", default_value = "json")]
    format: String,
}

fn load_dotenv() {
    for path in [".env"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let mut v = v.trim().to_string();
            if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
            {
                v = v[1..v.len() - 1].to_string();
            }
            if env::var(k).is_err() {
                env::set_var(k, v);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv();
    let args = Args::parse();
    let mut week_offset = args.week_offset;
    let mut out = args.out.clone();
    let mut out_explicit = args.out != "timetable.json";
    // back-compat: `cargo run -- out.json` / `cargo run -- +1`
    if let Some(p) = &args.positional_out {
        if let Ok(v) = p.parse::<i64>() {
            if week_offset == 0 && args.week.is_none() {
                week_offset = v;
            }
        } else if !out_explicit {
            out = p.clone();
            out_explicit = true;
        }
    }

    // --tests needs no login: local file only.
    if args.tests {
        return upcoming_tests(args.days);
    }

    let server = env::var("WEBUNTIS_SERVER").context("WEBUNTIS_SERVER not set")?;
    let school = env::var("WEBUNTIS_SCHOOL").context("WEBUNTIS_SCHOOL not set")?;
    let user = env::var("WEBUNTIS_USER").context("WEBUNTIS_USER not set (use .env)")?;
    let password =
        env::var("WEBUNTIS_PASSWORD").context("WEBUNTIS_PASSWORD not set (use .env)")?;
    // no --timetable -> personal (own timetable, unfiltered)
    let who: String = match &args.class {
        Some(c) => c.to_lowercase(),
        None => "personal".to_string(),
    };
    let class_filter: Option<String> = if args.all {
        None
    } else {
        args.class.clone().map(|c| c.to_lowercase())
    };

    // ---- date range ----
    let today = Local::now().date_naive();
    let (start, end): (NaiveDate, NaiveDate) = if let Some(d) = &args.date {
        let day = parse_date(d)?;
        (day, day)
    } else if args.today {
        (today, today)
    } else if args.tomorrow {
        let n = next_school_day(today);
        (n, n)
    } else if args.alerts {
        (today, next_school_day(today))
    } else if let Some(dw) = &args.day {
        let anchor = today + chrono::Duration::days(week_offset * 7);
        let (mon, _) = week_monday_sunday(anchor, 0);
        (weekday_in_week(mon, dw)?, weekday_in_week(mon, dw)?)
    } else if let Some(w) = &args.week {
        if w == "__current__" {
            week_monday_sunday(today, week_offset)
        } else {
            let anchor = parse_date(w)?;
            week_monday_sunday(anchor, 0)
        }
    } else {
        // default: current week; Sat/Sun roll to next week
        let off = if week_offset != 0 {
            week_offset
        } else if today.weekday().num_days_from_monday() >= 5 {
            1
        } else {
            0
        };
        week_monday_sunday(today, off)
    };
    let start_s = start.format("%Y%m%d").to_string();
    let end_s = end.format("%Y%m%d").to_string();
    eprintln!("range {start} .. {end}");

    // ---- session ----
    let base = format!("https://{server}/WebUntis/jsonrpc.do?school={school}");
    let client: Client = Client::builder()
        .cookie_store(true)
        .user_agent("timetable-scraper/0.1")
        .build()?;
    let auth: Value = rpc(&client, &base, "authenticate", json!({
        "user": user, "password": password, "client": "timetable-scraper"
    }))
    .await
    .context("Authentication failed. Are you offline?")?;

    // Teacher directory for full names (WENZ -> longname).
    let teacher_map = teacher_directory(&client, &base).await.unwrap_or_default();

    // ---- element: personal (own student) vs class ----
    let person_id = auth.get("personId").cloned();
    let person_type = auth.get("personType").cloned();
    let own_element: Option<Value> = match (&person_id, &person_type) {
        (Some(id), Some(t)) => Some(json!({ "id": id, "type": t })),
        _ => None,
    };
    // resolve --class via getKlassen (exact, else prefix like "4a"->4ag,4ar)
    let mut matched_names: Vec<String> = vec![];
    let mut fetch_elements: Vec<Value> = vec![];
    if let Some(cf) = &class_filter {
        if let Ok(klassen) = rpc(&client, &base, "getKlassen", json!({})).await {
            let ids = match_classes(&klassen, cf);
            matched_names = ids.iter().map(|(_, n)| n.to_lowercase()).collect();
            for (cid, _) in ids {
                fetch_elements.push(json!({ "id": cid, "type": 1 }));
            }
        }
    }
    if fetch_elements.is_empty() {
        fetch_elements.push(
            own_element.context("no personId in auth reply and no class match")?,
        );
    }

    let mut raw_all: Vec<Value> = vec![];
    for el in fetch_elements {
        let tt = fetch_timetable(&client, &base, el, &start_s, &end_s).await?;
        raw_all.extend(tt.as_array().cloned().unwrap_or_default());
    }
    // de-dup by id (prefix merges overlap on joint lessons)
    raw_all.sort_by_key(|p| p.get("id").and_then(|v| v.as_i64()).unwrap_or(0));
    raw_all.dedup_by_key(|p| p.get("id").and_then(|v| v.as_i64()).unwrap_or(0));

    // ---- slim + filter ----
    let mut lessons = slim_timetable(&Value::Array(raw_all), &teacher_map);
    if let Some(cf) = &class_filter {
        let names = if matched_names.is_empty() {
            vec![cf.clone()]
        } else {
            matched_names.clone()
        };
        lessons = lessons
            .into_iter()
            .filter(|l| {
                l.get("class_names")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter().any(|c| {
                            c.as_str()
                                .map(|s| {
                                    let s = s.to_lowercase();
                                    names.iter().any(|n| s == *n || s.starts_with(cf))
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            })
            .collect();
    }
    lessons.sort_by_key(|l| {
        (
            l.get("date").and_then(|v| v.as_i64()).unwrap_or(0),
            l.get("start").and_then(|v| v.as_i64()).unwrap_or(0),
        )
    });

    // ---- save: everything lands in timetables/ ----
    std::fs::create_dir_all("timetables")?;
    let pure_default = !out_explicit
        && args.date.is_none()
        && args.week.is_none()
        && !args.today
        && !args.tomorrow
        && !args.alerts
        && args.day.is_none()
        && args.period.is_none()
        && week_offset == 0
        && args.class.is_none()
        && !args.all;
    let out_path = if out_explicit {
        if out.contains('/') || out.contains('\\') {
            out.clone()
        } else {
            format!("timetables/{out}")
        }
    } else if start == end {
        // -d 30/10/2026 -> timetables/30-10-2026-personal.json
        format!("timetables/{}-{who}.json", start.format("%d-%m-%Y"))
    } else if pure_default {
        // no args -> canonical file, always overwritten
        "timetables/timetable.json".to_string()
    } else {
        // -w 30/10/2026 -> week of that day:
        // timetables/26-10-2026-01-11-2026-personal.json
        format!(
            "timetables/{}-{}-{who}.json",
            start.format("%d-%m-%Y"),
            end.format("%d-%m-%Y")
        )
    };
    let out_v = json!({
        "server": server, "school": school,
        "who": who,
        "range_start": start.to_string(), "range_end": end.to_string(),
        "class_filter": class_filter,
        "fetched_at": Local::now().to_rfc3339(),
        "lessons": lessons,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&out_v)?)?;
    eprintln!("saved {} lessons -> {out_path}", lessons.len());

    // ---- answer narrow queries ----
    if args.alerts {
        let alerts: Vec<Value> = lessons
            .into_iter()
            .filter(|l| {
                let code = l.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let sub = l.get("substText").and_then(|v| v.as_str()).unwrap_or("");
                code == "cancelled" || code == "irregular" || !sub.is_empty()
            })
            .collect();
        if alerts.is_empty() {
            println!("No cancellations or substitutions {start}..{end}.");
        } else {
            print_lessons(&alerts);
        }
    } else if let Some(n) = args.period {
        let day_lessons: Vec<&Value> = if lessons.len() > 1
            && lessons
                .iter()
                .map(|l| l.get("date").cloned().unwrap_or(Value::Null))
                .collect::<Vec<_>>()
                .windows(2)
                .any(|w| w[0] != w[1])
        {
            // multi-day + --period is ambiguous; pick the single queried day
            // (already narrowed by --day/--date/--today/--tomorrow) — if still
            // multi-day, use first date.
            let first = lessons.first().and_then(|l| l.get("date")).cloned();
            lessons
                .iter()
                .filter(|l| l.get("date") == first.as_ref())
                .collect()
        } else {
            lessons.iter().collect()
        };
        match day_lessons.get(n.saturating_sub(1)) {
            Some(l) => println!("{}", human_line(l)),
            None => println!("No period {n} (only {} lessons that day).", day_lessons.len()),
        }
    } else if args.format == "text" {
        print_lessons(&lessons);
    } else if args.today || args.tomorrow || args.date.is_some() || args.day.is_some() {
        // narrow day query defaults to readable text too
        print_lessons(&lessons);
    }

    let _ = rpc(&client, &base, "logout", json!({})).await;
    Ok(())
}

async fn fetch_timetable(
    client: &Client,
    base: &str,
    element: Value,
    start: &str,
    end: &str,
) -> Result<Value> {
    rpc(client, base, "getTimetable", json!({
        "options": {
            "id": Local::now().timestamp_millis(),
            "element": element,
            "startDate": start, "endDate": end,
            "showLsText": true, "showStudentgroup": true,
            "showLsNumber": true, "showSubstText": true,
            "showInfo": true, "showBooking": true,
            "klasseFields": ["id", "name", "longname"],
            "roomFields": ["id", "name", "longname"],
            "subjectFields": ["id", "name", "longname"],
            "teacherFields": ["id", "name", "longname"]
        }
    }))
    .await
}

async fn rpc(client: &Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({ "id": method, "method": method, "params": params, "jsonrpc": "2.0" });
    let resp = client.post(url).json(&body).send().await?;
    let v: Value = resp.json().await?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("JSON-RPC {method} error: {err}");
    }
    v.get("result")
        .cloned()
        .context(format!("no result in {method} reply"))
}

/// short teacher code -> full name, e.g. WENZ -> Wenzlik-ish longname.
async fn teacher_directory(client: &Client, base: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    // Local overrides first (server hides full names from students).
    for path in ["teachers.json"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        if k.starts_with('_') {
                            continue;
                        }
                        // rich teachers.json: {short, full-inc-title, full, last}
                        // or legacy flat {SHORT: "Full Name"}
                        let full = val
                            .get("full")
                            .and_then(|f| f.as_str())
                            .or_else(|| val.as_str());
                        if let Some(full) = full {
                            map.insert(k.to_uppercase(), full.to_string());
                        }
                    }
                }
            }
        }
    }
    let Ok(teachers) = rpc(client, base, "getTeachers", json!({})).await else {
        return Ok(map);
    };
    for t in teachers.as_array().cloned().unwrap_or_default() {
        if let (Some(short), Some(long)) = (
            t.get("name").and_then(|v| v.as_str()),
            t.get("longname").and_then(|v| v.as_str()),
        ) {
            map.insert(short.to_uppercase(), long.to_string());
        }
    }
    Ok(map)
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    let s = s.trim();
    for fmt in ["%d/%m/%Y", "%d.%m.%Y", "%Y-%m-%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d);
        }
    }
    anyhow::bail!("bad date '{s}', want DD/MM/YYYY")
}

/// Print upcoming entries from tests.json (Schularbeiten + Tests).
fn upcoming_tests(days: i64) -> Result<()> {
    let text = std::fs::read_to_string("tests.json").context(
        "no tests.json yet — create it: [{\"date\": \"20/10/2026\", \"subject\": \"Mathematik\", \"kind\": \"Schularbeit\", \"note\": \"\"}]",
    )?;
    let arr: Value = serde_json::from_str(&text)?;
    let today = Local::now().date_naive();
    let mut upcoming: Vec<(NaiveDate, String, String, String)> = vec![];
    for t in arr.as_array().cloned().unwrap_or_default() {
        let ds = t.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let Ok(d) = parse_date(ds) else {
            continue;
        };
        let diff = (d - today).num_days();
        if diff < 0 || diff > days {
            continue;
        }
        upcoming.push((
            d,
            t.get("subject").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            t.get("kind").and_then(|v| v.as_str()).unwrap_or("Test").to_string(),
            t.get("note").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ));
    }
    upcoming.sort();
    if upcoming.is_empty() {
        println!("No tests in the next {days} days.");
        return Ok(());
    }
    for (d, subj, kind, note) in upcoming {
        let diff = (d - today).num_days();
        let when = if diff == 0 {
            "TODAY".to_string()
        } else if diff == 1 {
            "tomorrow".to_string()
        } else {
            format!("in {diff} days")
        };
        let extra = if note.is_empty() { "".to_string() } else { format!(" — {note}") };
        println!("{} ({}): {subj} {kind}{extra}", d.format("%a %d.%m.%Y"), when);
    }
    Ok(())
}

fn match_classes(klassen: &Value, query: &str) -> Vec<(i64, String)> {
    let arr = klassen.as_array().cloned().unwrap_or_default();
    let q = query.to_lowercase();
    let mut exact = vec![];
    let mut prefix = vec![];
    for k in arr {
        let name = k.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let id = k.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if name.to_lowercase() == q {
            exact.push((id, name));
        } else if name.to_lowercase().starts_with(&q) {
            prefix.push((id, name));
        }
    }
    if !exact.is_empty() {
        exact
    } else {
        prefix
    }
}

fn slim_timetable(raw: &Value, teachers: &HashMap<String, String>) -> Vec<Value> {
    raw.as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let kl = p.get("kl").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let class_names: Vec<String> = kl
                .iter()
                .filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect();
            // teachers: prefer te[]; fallback to sg trailer "SUBJ_CLASS_TEACH"
            let mut teach_short: Vec<String> = p
                .get("te")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect();
            if teach_short.is_empty() {
                if let Some(sg) = p.get("sg").and_then(|v| v.as_str()) {
                    let parts: Vec<&str> = sg.split('_').collect();
                    if parts.len() >= 3 {
                        teach_short = parts[2..]
                            .join("_")
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
            }
            let teach_full: Vec<String> = teach_short
                .iter()
                .map(|s| {
                    teachers.get(&s.to_uppercase()).cloned().unwrap_or_else(|| s.clone())
                })
                .collect();
            let (subj_short, subj_long) = first_name_long(&p, "su");
            let (room_short, room_long) = first_name_long(&p, "ro");
            json!({
                "id": p.get("id"),
                "date": p.get("date"),
                "start": p.get("startTime"),
                "end": p.get("endTime"),
                "code": p.get("code"),
                "subject_short": subj_short, "subject": subj_long,
                "subjects": names(&p, "su"),
                "teacher_short": teach_short.get(0),
                "teacher": teach_full.get(0),
                "teachers": teach_short, "teacher_full": teach_full,
                "room_short": room_short, "room": room_long,
                "rooms": names(&p, "ro"),
                "class_names": class_names,
                "classes": names(&p, "kl"),
                "substText": p.get("substText"),
                "info": p.get("info"),
            })
        })
        .collect()
}

fn first_name_long(p: &Value, key: &str) -> (Option<String>, Option<String>) {
    let e = p.get(key).and_then(|v| v.as_array()).and_then(|a| a.first());
    (
        e.and_then(|v| v.get("name")).and_then(|v| v.as_str()).map(|s| s.to_string()),
        e.and_then(|v| v.get("longname")).and_then(|v| v.as_str()).map(|s| s.to_string()),
    )
}

fn names(period: &Value, key: &str) -> Vec<String> {
    period
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| {
            e.get("longname")
                .or_else(|| e.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

/// "You have Biologie with WENZ (Wenzlik) in BIO1 (E39), 08:00-08:50."
fn human_line(l: &Value) -> String {
    human_merged(l, 1, l.get("end").and_then(|v| v.as_i64()).unwrap_or(0))
}

/// e.g. "20261014 10:00-11:45: Doppelstunde Deutsch with NETE (Andrea Netek) in 225 (225)."
fn human_merged(first: &Value, count: usize, end: i64) -> String {
    let date = first.get("date").and_then(|v| v.as_i64()).unwrap_or(0);
    let s = first.get("start").and_then(|v| v.as_i64()).unwrap_or(0);
    let subj = first.get("subject").and_then(|v| v.as_str()).unwrap_or("?");
    let ts = first.get("teacher_short").and_then(|v| v.as_str()).unwrap_or("?");
    let tf = first.get("teacher").and_then(|v| v.as_str()).unwrap_or("?");
    let rs = first.get("room_short").and_then(|v| v.as_str()).unwrap_or("?");
    let rl = first.get("room").and_then(|v| v.as_str()).unwrap_or("?");
    let code = first.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let sub = first.get("substText").and_then(|v| v.as_str()).unwrap_or("");
    let flag = if code == "cancelled" {
        format!(" [CANCELLED: {sub}]")
    } else if code == "irregular" {
        format!(" [irregular: {sub}]")
    } else if !sub.is_empty() {
        format!(" ({sub})")
    } else {
        String::new()
    };
    let what = match count {
        1 => subj.to_string(),
        2 => format!("Doppelstunde {subj}"),
        3 => format!("Dreifachstunde {subj}"),
        n => format!("{n}x {subj}"),
    };
    format!(
        "{date} {}-{}: {what} with {ts} ({tf}) in {rs} ({rl}).{flag}",
        fmt_hhmm(s),
        fmt_hhmm(end),
    )
}

/// Merge consecutive lessons with same date/subject/teacher/room/code.
/// JSON is untouched — this only affects console output.
fn merge_consecutive(lessons: &[Value]) -> Vec<(&Value, usize, i64)> {
    fn key(l: &Value) -> String {
        serde_json::to_string(&(
            l.get("date"),
            l.get("subject"),
            l.get("teacher_short"),
            l.get("room_short"),
            l.get("code"),
            l.get("substText"),
        ))
        .unwrap_or_default()
    }
    let mut out: Vec<(&Value, usize, i64)> = vec![];
    for l in lessons {
        let end = l.get("end").and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(last) = out.last_mut() {
            if key(last.0) == key(l) {
                last.1 += 1;
                last.2 = end;
                continue;
            }
        }
        out.push((l, 1, end));
    }
    out
}

fn print_lessons(lessons: &[Value]) {
    for (first, count, end) in merge_consecutive(lessons) {
        println!("{}", human_merged(first, count, end));
    }
}

fn fmt_hhmm(v: i64) -> String {
    format!("{:02}:{:02}", v / 100, v % 100)
}

/// Sat/Sun + Fri afternoon roll to next Monday.
fn next_school_day(today: NaiveDate) -> NaiveDate {
    use chrono::Weekday::*;
    let wd = today.weekday();
    let now = Local::now().time();
    let fri_afternoon = wd == Fri && (now.hour() * 100 + now.minute()) >= 1400;
    match wd {
        Sat => today + chrono::Duration::days(2),
        Sun => today + chrono::Duration::days(1),
        Fri if fri_afternoon => today + chrono::Duration::days(3),
        _ => {
            let t = today + chrono::Duration::days(1);
            match t.weekday() {
                Sat => t + chrono::Duration::days(2),
                Sun => t + chrono::Duration::days(1),
                _ => t,
            }
        }
    }
}

fn week_monday(today: NaiveDate, offset: i64) -> (NaiveDate, NaiveDate) {
    let back = today.weekday().num_days_from_monday() as i64;
    let mon = today - chrono::Duration::days(back) + chrono::Duration::days(offset * 7);
    (mon, mon)
}

fn week_monday_sunday(today: NaiveDate, offset: i64) -> (NaiveDate, NaiveDate) {
    let (mon, _) = week_monday(today, offset);
    (mon, mon + chrono::Duration::days(6))
}

fn weekday_in_week(mon: NaiveDate, name: &str) -> Result<NaiveDate> {
    let n = name.to_lowercase();
    let idx: i64 = if n.starts_with("mon") {
        0
    } else if n.starts_with("tue") || n.starts_with("die") {
        1
    } else if n.starts_with("wed") || n.starts_with("mit") {
        2
    } else if n.starts_with("thu") || n.starts_with("don") {
        3
    } else if n.starts_with("fri") || n.starts_with("fre") {
        4
    } else if n.starts_with("sat") || n.starts_with("sam") {
        5
    } else if n.starts_with("sun") || n.starts_with("son") {
        6
    } else {
        anyhow::bail!("bad --day {name}, want mon..sun")
    };
    Ok(mon + chrono::Duration::days(idx))
}
