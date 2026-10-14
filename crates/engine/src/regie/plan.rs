//! `regie.plan`: understand a request (see [the module overview](super)).
//!
//! The request is read by rules, in German and English, so it works offline and gives the same
//! answer every time: format words ("16:9", "hochkant", "reel", "4K", "50p"), lengths ("etwa
//! drei Minuten", "30 seconds", "1:30 min"), dates ("dieses Jahr", "last summer", "im September
//! 2026"), places ("auf Mykonos", "from Paris"), the mood, a quoted title and the kind of film.
//! What the rules understood is listed in [`Plan::understood`], what they could not tell in
//! [`Plan::questions`]; defaults fill every gap, so the plan can always be built.

use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, str_p};
use crate::{Result, Session};

use super::types::Plan;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "regie.plan",
        label: "AI Director: Understand Request",
        menu: &[],
        shortcut: None,
        params: r#"{"request":str,"language":"de"|"en"? (default: detected),"today":"YYYY-MM-DD"? (default: the system date; for relative dates such as "this year")}"#,
        enabled: always,
        run: run_plan,
        journal: false,
    }]
}

fn run_plan(_s: &mut Session, p: &Value) -> Result<Value> {
    let request = str_p(p, "request").ok_or_else(|| bad("regie.plan", "`request` (text) is required"))?;
    if request.chars().count() > 4000 {
        return Err(bad("regie.plan", "the request is longer than 4000 characters"));
    }
    let today = match str_p(p, "today") {
        Some(t) => parse_date(t).ok_or_else(|| bad("regie.plan", format!("`today` must be YYYY-MM-DD, got {t:?}")))?,
        None => system_today(),
    };
    let mut plan = understand(request, today);
    if let Some(lang) = str_p(p, "language") {
        if lang != "de" && lang != "en" {
            return Err(bad("regie.plan", format!("`language` must be \"de\" or \"en\", got {lang:?}")));
        }
        if lang != plan.language {
            plan = Plan { language: lang.to_string(), ..plan };
            plan.understood = describe(&plan);
            plan.questions = open_questions(&plan);
        }
    }
    Ok(json!(plan))
}

/// A calendar date.
pub type Date = (i32, u32, u32);

/// Today's date (UTC) from the system clock.
pub fn system_today() -> Date {
    let secs = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = crate::media_browser::format_date(secs);
    parse_date(s.get(..10).unwrap_or("")).unwrap_or((2026, 1, 1))
}

/// `YYYY-MM-DD` with a valid month and day.
pub fn parse_date(s: &str) -> Option<Date> {
    let mut it = s.trim().split('-');
    let y: i32 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) || !(1..=9999).contains(&y) {
        return None;
    }
    Some((y, m, d))
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn fmt_date((y, m, d): Date) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

const MONTHS_DE: [&str; 12] = ["januar", "februar", "märz", "april", "mai", "juni", "juli", "august", "september", "oktober", "november", "dezember"];
const MONTHS_EN: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
const MONTH_NAMES_DE: [&str; 12] = ["Januar", "Februar", "März", "April", "Mai", "Juni", "Juli", "August", "September", "Oktober", "November", "Dezember"];
const MONTH_NAMES_EN: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

/// The month's name in `language` ("de" or "en"); `month` 1..=12.
pub fn month_name(month: u32, language: &str) -> &'static str {
    let names = if language == "de" { &MONTH_NAMES_DE } else { &MONTH_NAMES_EN };
    month.checked_sub(1).and_then(|i| names.get(i as usize)).copied().unwrap_or("")
}

/// One word of the request: its text as typed and lower-cased.
struct Word {
    raw: String,
    low: String,
}

/// Split into words: letters, digits and the characters that belong inside a number or a ratio
/// (`16:9`, `2,5`, `1:30`) stay together; everything else separates.
fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let inner = matches!(c, ':' | ',' | '.' | '×' | 'x' | '/')
            && i > 0
            && chars.get(i - 1).is_some_and(|p| p.is_ascii_digit())
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if c.is_alphanumeric() || *c == '-' && !cur.is_empty() || inner {
            cur.push(*c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .map(|raw| {
            let raw = raw.trim_end_matches('-').to_string();
            Word { low: raw.to_lowercase(), raw }
        })
        .filter(|w| !w.raw.is_empty())
        .flat_map(|w| {
            // "4K-Auflösung" also counts as "4K" and "Auflösung"
            let parts: Vec<Word> = if w.raw.contains('-') {
                w.raw.split('-').filter(|p| !p.is_empty()).map(|p| Word { raw: p.to_string(), low: p.to_lowercase() }).collect()
            } else {
                Vec::new()
            };
            std::iter::once(w).chain(parts)
        })
        .collect()
}

fn number_word(w: &str) -> Option<f64> {
    Some(match w {
        "ein" | "eine" | "einer" | "einen" | "one" | "a" | "an" => 1.0,
        "zwei" | "two" => 2.0,
        "drei" | "three" => 3.0,
        "vier" | "four" => 4.0,
        "fünf" | "fuenf" | "five" => 5.0,
        "sechs" | "six" => 6.0,
        "sieben" | "seven" => 7.0,
        "acht" | "eight" => 8.0,
        "neun" | "nine" => 9.0,
        "zehn" | "ten" => 10.0,
        "fünfzehn" | "fifteen" => 15.0,
        "zwanzig" | "twenty" => 20.0,
        "dreißig" | "dreissig" | "thirty" => 30.0,
        "vierzig" | "forty" => 40.0,
        "fünfzig" | "fifty" => 50.0,
        "sechzig" | "sixty" => 60.0,
        "neunzig" | "ninety" => 90.0,
        "anderthalb" | "eineinhalb" => 1.5,
        "zweieinhalb" => 2.5,
        "halbe" | "half" => 0.5,
        _ => return None,
    })
}

fn number(w: &str) -> Option<f64> {
    let v: f64 = w.replace(',', ".").parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// `m:ss` as seconds.
fn clock(w: &str) -> Option<f64> {
    let (m, s) = w.split_once(':')?;
    let (m, s): (u32, u32) = (m.parse().ok()?, s.parse().ok()?);
    (s < 60 && s.to_string().len() <= 2 && w.len() <= 6).then(|| f64::from(m) * 60.0 + f64::from(s))
}

fn is_minutes(w: &str) -> bool {
    matches!(w, "min" | "mins" | "minute" | "minuten" | "minutes")
}
fn is_seconds(w: &str) -> bool {
    matches!(w, "s" | "sek" | "sec" | "secs" | "sekunde" | "sekunden" | "second" | "seconds")
}

const GERMAN_MARKERS: &[&str] = &[
    "und",
    "der",
    "die",
    "das",
    "ein",
    "eine",
    "mein",
    "meine",
    "meinen",
    "meinem",
    "mach",
    "mache",
    "erstelle",
    "erstell",
    "bitte",
    "aus",
    "von",
    "diesem",
    "dieses",
    "jahr",
    "urlaub",
    "urlaube",
    "urlauben",
    "minuten",
    "sekunden",
    "fröhlich",
    "ruhig",
    "hochkant",
    "rückblick",
    "mit",
    "ohne",
    "für",
    "nur",
    "im",
    "auf",
    "einen",
];
const ENGLISH_MARKERS: &[&str] = &[
    "and", "the", "a", "my", "make", "create", "please", "from", "this", "year", "holiday", "holidays", "vacation", "minutes", "seconds", "happy", "calm",
    "vertical", "recap", "with", "without", "for", "only", "of", "on", "into",
];

/// Words that follow "in"/"auf"/"from" but are no place.
const NOT_PLACES: &[&str] = &[
    "dem",
    "der",
    "den",
    "die",
    "das",
    "einem",
    "einer",
    "einen",
    "meinem",
    "meinen",
    "meiner",
    "meine",
    "unserem",
    "unseren",
    "unserer",
    "diesem",
    "dieser",
    "diesen",
    "jedem",
    "allen",
    "the",
    "my",
    "our",
    "this",
    "that",
    "a",
    "an",
    "all",
    "every",
    "full",
    "hd",
    "uhd",
    "4k",
    "hochkant",
    "querformat",
    "hochformat",
    "format",
    "auflösung",
    "minuten",
    "sekunden",
    "musik",
    "urlaub",
    "urlauben",
    "urlaube",
    "film",
    "filme",
    "projekt",
    "video",
    "videos",
    "fotos",
    "bildern",
    "bilder",
    "sommer",
    "winter",
    "frühling",
    "frühjahr",
    "herbst",
    "summer",
    "spring",
    "autumn",
    "fall",
    "jahr",
    "year",
    "month",
    "monat",
    "zukunft",
    "ruhe",
    "farbe",
    "color",
    "colour",
    "schwarzweiß",
    "zeitlupe",
    "slow",
    "motion",
    "reel",
    "story",
    "instagram",
    "tiktok",
    "youtube",
    "high",
    "low",
    "order",
    "reihenfolge",
    "takt",
    "beat",
    "beats",
    "rhythmus",
    "minute",
    "sekunde",
    "zeit",
    "time",
    "kino",
    "cinema",
    "style",
    "stil",
    "englisch",
    "deutsch",
    "english",
    "german",
    "denen",
    "deren",
    "dessen",
    "welchen",
    "welcher",
    "welchem",
    "allem",
    "alles",
    "jeder",
    "mir",
    "mich",
    "uns",
    "euch",
    "ihnen",
    "ihr",
    "sie",
    "ihm",
    "ihn",
    "was",
    "wie",
    "datum",
    "date",
    "ort",
    "orte",
    "orten",
    "qualität",
    "länge",
    "stimmung",
    "rechnern",
    "rechner",
    "computer",
    "ordner",
    "folder",
    "folders",
    "material",
    "videomaterial",
    "clips",
    "aufnahmen",
    "handy",
    "phone",
    "kamera",
    "camera",
    "beginning",
    "anfang",
    "ende",
    "end",
];

/// What the rules make of `request` (dates relative to `today`).
pub fn understand(request: &str, today: Date) -> Plan {
    let w = words(request);
    let low: Vec<&str> = w.iter().map(|x| x.low.as_str()).collect();
    let has = |k: &str| low.contains(&k);
    let any = |ks: &[&str]| ks.iter().any(|k| low.contains(k));
    let contains = |k: &str| low.iter().any(|x| x.contains(k));

    let de = low.iter().filter(|x| GERMAN_MARKERS.contains(x)).count();
    let en = low.iter().filter(|x| ENGLISH_MARKERS.contains(x)).count();
    let language = if de >= en { "de" } else { "en" };

    // kind
    let kind = if any(&["reel", "reels", "tiktok", "shorts", "short", "story", "stories"]) {
        "reel"
    } else if contains("musikvideo") || any(&["musicvideo"]) || (has("music") && has("video") && (has("my") || has("song"))) || contains("songvideo") {
        "musicVideo"
    } else if contains("diashow") || contains("slideshow") || contains("fotoshow") {
        "slideshow"
    } else {
        "recap"
    };

    // aspect
    let mut aspect = if kind == "reel" { "9:16" } else { "16:9" };
    let mut aspect_said = false;
    for (word, a) in [("16:9", "16:9"), ("9:16", "9:16"), ("1:1", "1:1"), ("4:5", "4:5")] {
        if has(word) {
            aspect = a;
            aspect_said = true;
        }
    }
    if !aspect_said {
        if any(&["hochkant", "hochformat", "vertical", "portrait", "vertikal"]) {
            aspect = "9:16";
            aspect_said = true;
        } else if any(&["quadrat", "quadratisch", "square"]) {
            aspect = "1:1";
            aspect_said = true;
        } else if any(&["querformat", "landscape", "widescreen", "breitbild"]) {
            aspect = "16:9";
            aspect_said = true;
        }
    }

    // resolution
    let mut tier = 1080;
    let mut tier_said = false;
    if any(&["4k", "uhd", "2160", "2160p", "ultra"]) || contains("3840") {
        tier = 2160;
        tier_said = true;
    } else if any(&["720", "720p"]) {
        tier = 720;
        tier_said = true;
    } else if any(&["1080", "1080p", "fullhd", "hd"]) || (has("full") && has("hd")) {
        tier = 1080;
        tier_said = true;
    }
    let (width, height) = frame_size(aspect, tier);

    // frame rate: "25 fps", "50p", "30 bilder pro sekunde"
    // phones record 30 frames per second: their footage plays without judder at 30
    let mut fps = 30.0;
    let mut fps_said = false;
    for (i, x) in low.iter().enumerate() {
        let next = low.get(i + 1).copied().unwrap_or("");
        let candidate = if let Some(n) = x.strip_suffix('p').filter(|n| matches!(*n, "24" | "25" | "30" | "50" | "60")) {
            number(n)
        } else if matches!(next, "fps" | "bps" | "bilder" | "frames" | "vollbilder") {
            number(x)
        } else {
            None
        };
        if let Some(f) = candidate.filter(|f| (1.0..=240.0).contains(f)) {
            fps = f;
            fps_said = true;
        }
    }

    // length
    let mut seconds = None;
    for (i, x) in low.iter().enumerate() {
        let next = low.get(i + 1).copied().unwrap_or("");
        let value = clock(x).filter(|_| is_minutes(next)).map(|v| (v, true)).or_else(|| {
            let n = number(x).or_else(|| number_word(x))?;
            if is_minutes(next) {
                Some((n * 60.0, true))
            } else if is_seconds(next) {
                Some((n, true))
            } else if next == "halbe" && low.get(i + 2).copied().is_some_and(is_minutes) {
                Some((n * 30.0, true))
            } else {
                None
            }
        });
        if let Some((v, _)) = value.filter(|(v, _)| (1.0..=7200.0).contains(v)) {
            seconds = Some(v);
        }
    }
    if seconds.is_none() && kind == "reel" {
        seconds = Some(30.0);
    }

    // dates
    let (y, _, _) = today;
    let mut from = None;
    let mut to = None;
    let year_of = |k: usize| -> Option<i32> { low.get(k).and_then(|x| x.parse::<i32>().ok()).filter(|v| (1900..=2100).contains(v)) };
    let mut year = None;
    for (i, _) in low.iter().enumerate() {
        if let Some(v) = year_of(i) {
            year = Some(v);
        }
    }
    let this_year = (has("dieses") || has("diesem") || has("diesen") || has("this") || has("heuer")) && any(&["jahr", "jahres", "year", "jahr's"])
        || contains("diesjährig");
    let last_year = any(&["letztes", "letzten", "vergangenes", "vergangenen", "last"]) && any(&["jahr", "year", "jahres"]);
    if this_year {
        year = Some(y);
    } else if last_year {
        year = Some(y - 1);
    }
    let mut month = None;
    for x in &low {
        let norm = x.replace("maerz", "märz");
        if let Some(i) = MONTHS_DE.iter().position(|m| *m == norm).or_else(|| MONTHS_EN.iter().position(|m| *m == norm)) {
            // "may" is also a verb: only with a year or "im"/"in"
            if norm == "may" && year.is_none() && !has("in") {
                continue;
            }
            month = Some(i as u32 + 1);
        }
    }
    let season = if any(&["sommer", "summer", "sommerurlaub"]) || contains("sommerurlaub") {
        Some((6, 8))
    } else if any(&["frühling", "frühjahr", "spring"]) {
        Some((3, 5))
    } else if any(&["herbst", "autumn"]) || (language == "en" && has("fall") && any(&["last", "this"])) {
        Some((9, 11))
    } else if any(&["winter"]) {
        Some((12, 2))
    } else {
        None
    };
    // "letzten Sommer" / "last summer": the latest one that is over
    let last_season = any(&["letzten", "letzter", "vergangenen", "last"]) && season.is_some() && year.is_none();
    let ref_year = match (year, season) {
        (Some(v), _) => v,
        (None, Some((_, end))) if last_season => {
            let (_, m, _) = today;
            match end {
                // a winter is named after the year it ends in
                2 if m >= 3 || m == 12 => y,
                2 => y - 1,
                _ if m > end => y,
                _ => y - 1,
            }
        }
        _ => y,
    };
    if let Some(m) = month {
        from = Some((ref_year, m, 1));
        to = Some((ref_year, m, days_in_month(ref_year, m)));
    } else if let Some((a, b)) = season {
        if a == 12 {
            from = Some((ref_year - 1, 12, 1));
            to = Some((ref_year, 2, days_in_month(ref_year, 2)));
        } else {
            from = Some((ref_year, a, 1));
            to = Some((ref_year, b, days_in_month(ref_year, b)));
        }
    } else if let Some(yy) = year {
        from = Some((yy, 1, 1));
        to = Some((yy, 12, 31));
    }

    // places: words after a preposition that are not a known non-place
    let mut places: Vec<String> = Vec::new();
    for (i, x) in low.iter().enumerate() {
        if !matches!(*x, "auf" | "in" | "aus" | "von" | "nach" | "at" | "from" | "on" | "to" | "into") {
            continue;
        }
        let Some(next) = w.get(i + 1) else { continue };
        let candidate = next.low.as_str();
        let first = next.raw.chars().next();
        let looks_like_name = first.is_some_and(char::is_uppercase) || matches!(*x, "auf" | "nach" | "aus");
        if !looks_like_name
            || candidate.chars().any(|c| c.is_ascii_digit())
            || candidate.len() < 3
            || NOT_PLACES.contains(&candidate)
            || MONTHS_DE.contains(&candidate)
            || MONTHS_EN.contains(&candidate)
            || GERMAN_MARKERS.contains(&candidate)
            || ENGLISH_MARKERS.contains(&candidate)
        {
            continue;
        }
        let name = next.raw.clone();
        if !places.iter().any(|p| p.eq_ignore_ascii_case(&name)) {
            places.push(name);
        }
    }

    // mood
    let mood = if any(&["ruhig", "ruhiger", "entspannt", "sanft", "romantisch", "emotional", "calm", "chill", "relaxed", "gentle", "romantic", "besinnlich"]) {
        "calm"
    } else if any(&["episch", "epic", "cinematic", "kinomäßig", "dramatisch", "dramatic", "kino"]) {
        "epic"
    } else if kind == "slideshow" {
        "calm"
    } else {
        "upbeat"
    };

    // title: quoted text, or after "Titel"/"title"/"called"
    let title = quoted(request).or_else(|| {
        let i = low.iter().position(|x| matches!(*x, "titel" | "title" | "called" | "named" | "heißt" | "heissen"))?;
        let rest: Vec<&str> = w.iter().skip(i + 1).take_while(|x| !matches!(x.low.as_str(), "und" | "and" | "mit" | "with")).map(|x| x.raw.as_str()).collect();
        (!rest.is_empty()).then(|| rest.join(" "))
    });

    let videos_only = (has("nur") || has("only")) && any(&["videos", "video", "clips"]) && !any(&["fotos", "photos", "bilder"]);
    let photos_only = (has("nur") || has("only")) && any(&["fotos", "photos", "bilder", "pictures"]) && !any(&["videos", "clips"]);
    let no_music = contains("ohne musik") || (has("ohne") && has("musik")) || (has("no") && has("music")) || (has("without") && has("music"));

    let folders: Vec<String> = request
        .split_whitespace()
        .filter(|t| t.starts_with('/') || t.starts_with("~/"))
        .map(|t| t.trim_matches(|c| c == ',' || c == '.').to_string())
        .collect();
    let music = folders.iter().find(|f| has_audio_extension(f)).cloned();
    let folders: Vec<String> = folders.into_iter().filter(|f| !has_audio_extension(f)).collect();

    let mut plan = Plan {
        request: request.trim().to_string(),
        language: language.into(),
        kind: kind.into(),
        aspect: aspect.into(),
        width,
        height,
        fps,
        seconds,
        from: from.map(fmt_date),
        to: to.map(fmt_date),
        places,
        mood: mood.into(),
        title,
        music: if no_music { Some(String::new()) } else { music },
        photos: !videos_only,
        videos: !photos_only,
        folders,
        understood: Vec::new(),
        questions: Vec::new(),
    };
    let _ = (aspect_said, tier_said, fps_said);
    plan.understood = describe(&plan);
    plan.questions = open_questions(&plan);
    plan
}

fn has_audio_extension(path: &str) -> bool {
    let low = path.to_lowercase();
    [".mp3", ".wav", ".m4a", ".aac", ".flac", ".ogg", ".opus"].iter().any(|e| low.ends_with(e))
}

/// Text inside „…“, "…", “…” or «…».
fn quoted(text: &str) -> Option<String> {
    for (open, close) in [('„', '“'), ('“', '”'), ('"', '"'), ('«', '»'), ('»', '«')] {
        if let Some(a) = text.find(open) {
            let rest = text.get(a + open.len_utf8()..)?;
            if let Some(b) = rest.find(close) {
                let t = rest.get(..b)?.trim();
                if !t.is_empty() && t.chars().count() <= 120 {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Frame size for `aspect` at `tier` (720, 1080 or 2160 lines on the short side).
pub fn frame_size(aspect: &str, tier: u32) -> (u32, u32) {
    let short = match tier {
        2160 => 2160,
        720 => 720,
        _ => 1080,
    };
    let long = short * 16 / 9;
    match aspect {
        "9:16" => (short, long),
        "1:1" => (short, short),
        "4:5" => (short, short * 5 / 4),
        _ => (long, short),
    }
}

/// The plan as lines for the user, in its language.
pub fn describe(p: &Plan) -> Vec<String> {
    let de = p.language == "de";
    let mut out = Vec::new();
    let res = match (p.width.max(p.height), p.width.min(p.height)) {
        (_, 2160) => "4K",
        (_, 1080) => "Full HD",
        (_, 720) => "HD",
        _ => "",
    };
    let orient = match (p.aspect.as_str(), de) {
        ("9:16", true) => "Hochformat",
        ("9:16", false) => "vertical",
        ("1:1", true) => "quadratisch",
        ("1:1", false) => "square",
        ("4:5", true) => "Hochformat 4:5",
        ("4:5", false) => "portrait 4:5",
        (_, true) => "Querformat",
        (_, false) => "landscape",
    };
    let fps = if (p.fps - p.fps.round()).abs() < 1e-9 { format!("{}", p.fps.round() as i64) } else { format!("{:.3}", p.fps) };
    out.push(if de {
        format!("Format: {} ({orient}), {} × {} ({res}), {fps} Bilder pro Sekunde", p.aspect, p.width, p.height)
    } else {
        format!("Format: {} ({orient}), {} × {} ({res}), {fps} frames per second", p.aspect, p.width, p.height)
    });
    let kind = match (p.kind.as_str(), de) {
        ("reel", true) => "Reel (kurz, schnell geschnitten)",
        ("reel", false) => "reel (short, fast cuts)",
        ("slideshow", true) => "Diashow (ruhig, Fotos zuerst)",
        ("slideshow", false) => "slideshow (calm, photos first)",
        ("musicVideo", true) => "Musikvideo (auf den Song geschnitten)",
        ("musicVideo", false) => "music video (cut to the song)",
        (_, true) => "Rückblick (Kapitel nach Datum und Ort)",
        (_, false) => "recap (chapters by date and place)",
    };
    out.push(if de { format!("Art: {kind}") } else { format!("Kind: {kind}") });
    out.push(match (p.seconds, de) {
        (Some(s), true) => format!("Länge: etwa {}", mmss(s)),
        (Some(s), false) => format!("Length: about {}", mmss(s)),
        (None, true) => "Länge: automatisch (nach Musik und Material)".into(),
        (None, false) => "Length: automatic (from the music and the media)".into(),
    });
    match (&p.from, &p.to) {
        (Some(a), Some(b)) => out.push(if de { format!("Zeitraum: {} bis {}", de_date(a), de_date(b)) } else { format!("Dates: {a} to {b}") }),
        (Some(a), None) => out.push(if de { format!("Zeitraum: ab {}", de_date(a)) } else { format!("Dates: from {a}") }),
        (None, Some(b)) => out.push(if de { format!("Zeitraum: bis {}", de_date(b)) } else { format!("Dates: until {b}") }),
        (None, None) => out.push(if de { "Zeitraum: alles, was ich finde".into() } else { "Dates: everything I find".into() }),
    }
    if !p.places.is_empty() {
        out.push(if de { format!("Orte: {}", p.places.join(", ")) } else { format!("Places: {}", p.places.join(", ")) });
    }
    let mood = match (p.mood.as_str(), de) {
        ("calm", true) => "ruhig",
        ("calm", false) => "calm",
        ("epic", true) => "episch",
        ("epic", false) => "epic",
        (_, true) => "fröhlich, schwungvoll",
        (_, false) => "upbeat",
    };
    out.push(if de { format!("Stimmung: {mood}") } else { format!("Mood: {mood}") });
    let media = match (p.videos, p.photos, de) {
        (true, false, true) => "nur Videos",
        (true, false, false) => "videos only",
        (false, true, true) => "nur Fotos",
        (false, true, false) => "photos only",
        (_, _, true) => "Videos und Fotos",
        (_, _, false) => "videos and photos",
    };
    out.push(if de { format!("Material: {media}") } else { format!("Media: {media}") });
    out.push(match (&p.title, de) {
        (Some(t), true) => format!("Titel: „{t}“"),
        (Some(t), false) => format!("Title: “{t}”"),
        (None, true) => "Titel: mache ich aus dem Inhalt".into(),
        (None, false) => "Title: made from the content".into(),
    });
    out.push(match (p.music.as_deref(), de) {
        (Some(""), true) => "Musik: keine".into(),
        (Some(""), false) => "Music: none".into(),
        (Some(m), true) => format!("Musik: {m}"),
        (Some(m), false) => format!("Music: {m}"),
        (None, true) => "Musik: die erste passende Musikdatei in deinen Ordnern, Schnitte auf den Beat".into(),
        (None, false) => "Music: the first suitable music file in your folders, cuts on the beat".into(),
    });
    if !p.folders.is_empty() {
        out.push(if de { format!("Ordner: {}", p.folders.join(", ")) } else { format!("Folders: {}", p.folders.join(", ")) });
    }
    out
}

fn open_questions(p: &Plan) -> Vec<String> {
    let de = p.language == "de";
    let mut q = Vec::new();
    if p.kind == "recap" && p.from.is_none() && p.to.is_none() && p.places.is_empty() {
        q.push(if de {
            "Welcher Zeitraum oder welcher Ort? Ohne Angabe nehme ich alles, was ich in deinen Ordnern finde.".into()
        } else {
            "Which dates or place? Without one I use everything I find in your folders.".into()
        });
    }
    q
}

fn mmss(s: f64) -> String {
    let t = s.max(0.0).round() as u64;
    format!("{}:{:02} min", t / 60, t % 60)
}

fn de_date(iso: &str) -> String {
    match parse_date(iso) {
        Some((y, m, d)) => format!("{d:02}.{m:02}.{y}"),
        None => iso.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: Date = (2026, 10, 15);

    #[test]
    fn dirks_holiday_request() {
        let p = understand(
            "Ich möchte ein neues Film Projekt in 16:9 in 4K-Auflösung erstellen, suche mir aus meinem Videomaterial auf meinen Rechnern Clips raus, aus denen du einen Urlaubsrecap-Film über meine bisherigen Urlaube dieses Jahr erstellst.",
            TODAY,
        );
        assert_eq!(p.language, "de");
        assert_eq!(p.kind, "recap");
        assert_eq!((p.aspect.as_str(), p.width, p.height), ("16:9", 3840, 2160));
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2026-01-01"), Some("2026-12-31")));
        assert!(p.places.is_empty(), "{:?}", p.places);
        assert_eq!(p.mood, "upbeat");
        assert!(p.understood.iter().any(|l| l.contains("3840 × 2160")), "{:?}", p.understood);
    }

    #[test]
    fn reel_defaults_to_vertical_and_thirty_seconds() {
        let p = understand("Make an Instagram reel from my Mykonos clips, fast and happy", TODAY);
        assert_eq!(p.language, "en");
        assert_eq!((p.kind.as_str(), p.aspect.as_str(), p.width, p.height), ("reel", "9:16", 1080, 1920));
        assert_eq!(p.seconds, Some(30.0));
        assert_eq!(p.fps, 30.0);
    }

    #[test]
    fn lengths_in_words_numbers_and_clock_form() {
        assert_eq!(understand("ein Rückblick, etwa drei Minuten lang", TODAY).seconds, Some(180.0));
        assert_eq!(understand("make it 45 seconds", TODAY).seconds, Some(45.0));
        assert_eq!(understand("bitte 1:30 min", TODAY).seconds, Some(90.0));
        assert_eq!(understand("anderthalb Minuten", TODAY).seconds, Some(90.0));
        assert_eq!(understand("2,5 Minuten", TODAY).seconds, Some(150.0));
        assert_eq!(understand("16:9 Film", TODAY).seconds, None, "a ratio is no length");
    }

    #[test]
    fn dates_months_seasons_and_years() {
        let p = understand("Urlaub auf Mykonos im September 2026", TODAY);
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2026-09-01"), Some("2026-09-30")));
        assert_eq!(p.places, vec!["Mykonos".to_string()]);
        let p = understand("our trips last summer", TODAY);
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2026-06-01"), Some("2026-08-31")), "in October, last summer is this year's");
        let p = understand("unsere Reise letzten Winter", TODAY);
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2025-12-01"), Some("2026-02-28")));
        let p = understand("Winter 2026 in den Bergen", TODAY);
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2025-12-01"), Some("2026-02-28")));
        let p = understand("alles aus 2024", TODAY);
        assert_eq!((p.from.as_deref(), p.to.as_deref()), (Some("2024-01-01"), Some("2024-12-31")));
    }

    #[test]
    fn format_words_mood_title_and_media() {
        let p = understand("Hochkant, 50p, ruhig, nur Fotos, Titel „Oma wird 80“ bitte", TODAY);
        assert_eq!(p.aspect, "9:16");
        assert_eq!(p.fps, 50.0);
        assert_eq!(p.mood, "calm");
        assert_eq!(p.title.as_deref(), Some("Oma wird 80"));
        assert!(p.photos && !p.videos);
        let p = understand("square slideshow without music", TODAY);
        assert_eq!((p.aspect.as_str(), p.kind.as_str(), p.music.as_deref()), ("1:1", "slideshow", Some("")));
    }

    #[test]
    fn hostile_text_is_handled() {
        for s in ["", "   ", "::::", "16:9:9:9", "999999999999999999999 Minuten", "in", "auf", "„", "„“", "-", "1:", ":30 min", "\u{0}\u{1}"] {
            let p = understand(s, TODAY);
            assert!(p.width > 0 && p.height > 0 && p.fps > 0.0, "{s:?}");
            assert!(p.seconds.is_none_or(|v| (1.0..=7200.0).contains(&v)), "{s:?}");
        }
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("2024-02-29"), Some((2024, 2, 29)));
    }
}
