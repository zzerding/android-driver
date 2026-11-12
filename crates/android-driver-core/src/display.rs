/// Keyguard observations are kept conservative: a state is unlocked only when
/// every recognized keyguard field agrees, and an unknown field value makes
/// the whole state unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyguard {
    Unlocked,
    Locked,
    Unknown,
}

const KEYGUARD_FIELDS: [&str; 5] = [
    "showing",
    "mshowinglockscreen",
    "mkeyguardshowing",
    "keyguardshowing",
    "iskeyguardlocked",
];

pub fn keyguard_state(dump: &str) -> Keyguard {
    let lines: Vec<&str> = dump.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|line| line.trim() == "KeyguardServiceDelegate")
    else {
        return Keyguard::Unknown;
    };
    let indent = lines[start].len() - lines[start].trim_start().len();
    let mut saw_value = false;
    let mut saw_locked = false;
    let mut saw_unlocked = false;
    let mut seen_keys = 0usize;
    for line in &lines[start + 1..] {
        if line.trim().is_empty() {
            continue;
        }
        let line_indent = line.len() - line.trim_start().len();
        if line_indent <= indent {
            break;
        }
        let Some((key, value)) = split_key_value(line.trim()) else {
            continue;
        };
        let key_lower = key.to_ascii_lowercase();
        if !KEYGUARD_FIELDS.contains(&key_lower.as_str()) {
            continue;
        }
        seen_keys += 1;
        let Some(locked) = parse_bool(value) else {
            return Keyguard::Unknown;
        };
        saw_value = true;
        if locked {
            saw_locked = true;
        } else {
            saw_unlocked = true;
        }
    }
    if !saw_value || seen_keys == 0 {
        return Keyguard::Unknown;
    }
    if saw_locked && saw_unlocked {
        Keyguard::Unknown
    } else if saw_locked {
        Keyguard::Locked
    } else {
        Keyguard::Unlocked
    }
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    if let Some((key, value)) = line.split_once('=') {
        return Some((key.trim(), value.trim()));
    }
    if let Some((key, value)) = line.split_once(':') {
        return Some((key.trim(), value.trim()));
    }
    None
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedActivity {
    pub component: String,
    pub user: u32,
    pub task: u32,
}

impl ResumedActivity {
    pub fn package(&self) -> &str {
        self.component
            .split_once('/')
            .map_or(self.component.as_str(), |(package, _)| package)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayFocus {
    Missing,
    Present(Option<ResumedActivity>),
    Uncertain(String),
}

/// Parses one display section of `dumpsys activity activities`. Display
/// sections end at the next display header or at the global "Resumed
/// activities in task display areas" summary, so the physical display's
/// resumed activity is never attributed to the virtual display.
pub fn display_focus(dump: &str, display_id: u32) -> DisplayFocus {
    let mut found = false;
    let mut inside = false;
    let mut resumed = Vec::new();
    let mut uncertain = None;

    for line in dump.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Resumed activities in task display areas") {
            if inside {
                break;
            }
            continue;
        }
        if let Some(header_id) = parse_display_header(trimmed) {
            if header_id == display_id {
                if found {
                    return DisplayFocus::Uncertain(format!(
                        "display #{display_id} appears more than once"
                    ));
                }
                found = true;
                inside = true;
            } else if inside {
                break;
            }
            continue;
        }
        if trimmed.starts_with("Display #") {
            return DisplayFocus::Uncertain(format!("cannot parse display header line: {trimmed}"));
        }
        if !inside {
            continue;
        }
        match parse_resumed_line(trimmed) {
            Ok(items) => resumed.extend(items),
            Err(error) => {
                uncertain.get_or_insert(error);
            }
        }
    }

    if let Some(error) = uncertain {
        return DisplayFocus::Uncertain(error);
    }
    if !found {
        return DisplayFocus::Missing;
    }
    resumed.sort_by(|left, right| {
        (&left.component, left.user, left.task).cmp(&(&right.component, right.user, right.task))
    });
    resumed.dedup();
    match resumed.len() {
        0 => DisplayFocus::Present(None),
        1 => DisplayFocus::Present(Some(resumed.remove(0))),
        _ => DisplayFocus::Uncertain("conflicting resumed activities in one display".to_owned()),
    }
}

fn parse_display_header(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("Display #")?;
    let digits_end = rest.find(|character: char| !character.is_ascii_digit())?;
    if digits_end == 0 || !line.trim_end().ends_with("):") {
        return None;
    }
    rest[..digits_end].parse().ok()
}

fn parse_resumed_line(line: &str) -> std::result::Result<Vec<ResumedActivity>, String> {
    let mut activities = Vec::new();
    for marker in ["topResumedActivity", "mResumedActivity"] {
        let mut search_from = 0;
        while let Some(offset) = line[search_from..].find(marker) {
            let start = search_from + offset;
            search_from = start + marker.len();
            let after = line[search_from..].trim_start();
            let after = after.trim_start_matches(['=', ':', ' ']).trim_start();
            let Some(record) = after.strip_prefix("ActivityRecord{") else {
                return Err(format!(
                    "resumed activity marker without an ActivityRecord on: {line}"
                ));
            };
            let Some(end) = record.find('}') else {
                return Err(format!("unterminated ActivityRecord on: {line}"));
            };
            activities.push(parse_record_body(&record[..end])?);
        }
    }
    Ok(activities)
}

fn parse_record_body(body: &str) -> std::result::Result<ResumedActivity, String> {
    let tokens: Vec<&str> = body.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        let Some(user_text) = token.strip_prefix('u') else {
            continue;
        };
        if user_text.is_empty() || !user_text.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Some(component) = tokens.get(index + 1) else {
            continue;
        };
        if !component.contains('/') {
            continue;
        }
        let Some(task) = tokens.get(index + 2) else {
            continue;
        };
        let Some(task_text) = task.strip_prefix('t') else {
            continue;
        };
        let task_text = task_text.trim_matches(|character| character == '}' || character == ',');
        if task_text.is_empty() || !task_text.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        return Ok(ResumedActivity {
            component: (*component).to_owned(),
            user: user_text
                .parse()
                .map_err(|_| format!("invalid user in ActivityRecord: {body}"))?,
            task: task_text
                .parse()
                .map_err(|_| format!("invalid task in ActivityRecord: {body}"))?,
        });
    }
    Err(format!(
        "cannot find user/component/task in ActivityRecord: {body}"
    ))
}

pub fn parse_wm_size(dump: &str) -> std::result::Result<(u32, u32), String> {
    let mut physical = None;
    let mut override_size = None;
    for line in dump.lines() {
        let lower = line.to_ascii_lowercase();
        let value = if lower.contains("override size:") {
            Some(&mut override_size)
        } else if lower.contains("physical size:") {
            Some(&mut physical)
        } else {
            None
        };
        let Some(slot) = value else {
            continue;
        };
        let Some((_, value)) = line.rsplit_once(':') else {
            return Err(format!("cannot parse display size line: {line}"));
        };
        *slot = Some(parse_dimensions(value.trim())?);
    }
    override_size
        .or(physical)
        .ok_or_else(|| "dumpsys wm size did not contain a physical or override size".to_owned())
}

fn parse_dimensions(value: &str) -> std::result::Result<(u32, u32), String> {
    let Some((width, height)) = value.split_once('x') else {
        return Err(format!("display size is not WIDTHxHEIGHT: {value}"));
    };
    let width = width
        .trim()
        .parse()
        .map_err(|_| format!("invalid display width: {value}"))?;
    let height = height
        .trim()
        .parse()
        .map_err(|_| format!("invalid display height: {value}"))?;
    Ok((width, height))
}

pub fn parse_wm_density(dump: &str) -> std::result::Result<u32, String> {
    let mut physical = None;
    let mut override_density = None;
    for line in dump.lines() {
        let lower = line.to_ascii_lowercase();
        let target = if lower.contains("override density:") {
            Some(&mut override_density)
        } else if lower.contains("physical density:") {
            Some(&mut physical)
        } else {
            None
        };
        let Some(slot) = target else {
            continue;
        };
        let Some((_, value)) = line.rsplit_once(':') else {
            return Err(format!("cannot parse display density line: {line}"));
        };
        let value = value.trim();
        let digits: String = value
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return Err(format!("invalid display density: {value}"));
        }
        *slot = Some(
            digits
                .parse()
                .map_err(|_| format!("invalid display density: {value}"))?,
        );
    }
    override_density.or(physical).ok_or_else(|| {
        "dumpsys wm density did not contain a physical or override density".to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVITIES: &str = "\
ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)
Display #0 (activities from top to bottom):
  * Task{7c4a2f1 #1 type=home visible=true}
  mResumedActivity: ActivityRecord{1a2b3c4 u0 com.android.launcher3/.Launcher t1}
  topResumedActivity=ActivityRecord{1a2b3c4 u0 com.android.launcher3/.Launcher t1}
Display #7 (activities from top to bottom):
  topResumedActivity=ActivityRecord{9f8e7d6 u10 com.example.app/.Main t42}
  Resumed activities in task display areas
";

    #[test]
    fn parses_one_display_without_leaking_into_the_next() {
        assert_eq!(
            display_focus(ACTIVITIES, 0),
            DisplayFocus::Present(Some(ResumedActivity {
                component: "com.android.launcher3/.Launcher".into(),
                user: 0,
                task: 1,
            }))
        );
        assert_eq!(
            display_focus(ACTIVITIES, 7),
            DisplayFocus::Present(Some(ResumedActivity {
                component: "com.example.app/.Main".into(),
                user: 10,
                task: 42,
            }))
        );
        assert_eq!(display_focus(ACTIVITIES, 3), DisplayFocus::Missing);
    }

    #[test]
    fn duplicate_and_conflicting_resumed_activities_are_uncertain() {
        let conflicting = "\
Display #7 (activities from top to bottom):
  mResumedActivity: ActivityRecord{a u0 com.one/.A t1}
  topResumedActivity=ActivityRecord{b u0 com.two/.B t2}
";
        assert!(matches!(
            display_focus(conflicting, 7),
            DisplayFocus::Uncertain(_)
        ));

        let malformed = "\
Display #7 (activities from top to bottom):
  mResumedActivity: ActivityRecord{bad record}
";
        assert!(matches!(
            display_focus(malformed, 7),
            DisplayFocus::Uncertain(_)
        ));
    }

    #[test]
    fn display_without_resumed_activity_is_present_but_empty() {
        let dump = "Display #7 (activities from top to bottom):\n  * Task{x #1}\n";
        assert_eq!(display_focus(dump, 7), DisplayFocus::Present(None));
    }

    #[test]
    fn keyguard_requires_agreeing_recognized_fields() {
        assert_eq!(
            keyguard_state(
                "KeyguardServiceDelegate\n  showing=false\n  mShowingLockscreen=false\n"
            ),
            Keyguard::Unlocked
        );
        assert_eq!(
            keyguard_state("unrelated:\n  showing=false\n"),
            Keyguard::Unknown
        );
        assert_eq!(
            keyguard_state("KeyguardServiceDelegate\n  showing=true\n  mShowingLockscreen=true\n"),
            Keyguard::Locked
        );
        assert_eq!(
            keyguard_state("KeyguardServiceDelegate\n  showing=true\n  mShowingLockscreen=false\n"),
            Keyguard::Unknown
        );
        assert_eq!(
            keyguard_state("KeyguardServiceDelegate\n  showing=maybe\n"),
            Keyguard::Unknown
        );
        assert_eq!(keyguard_state("nothing relevant\n"), Keyguard::Unknown);
    }

    #[test]
    fn wm_parsers_prefer_override_values() {
        let size = "Physical size: 1080x2400\nOverride size: 720x1600\n";
        assert_eq!(parse_wm_size(size).unwrap(), (720, 1600));
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\n").unwrap(),
            (1080, 2400)
        );
        assert!(parse_wm_size("nothing\n").is_err());
        assert!(parse_wm_size("Physical size: 1080x\n").is_err());

        let density = "Physical density: 420\nOverride density: 320dpi\n";
        assert_eq!(parse_wm_density(density).unwrap(), 320);
        assert_eq!(parse_wm_density("Physical density: 420\n").unwrap(), 420);
        assert!(parse_wm_density("Physical density: high\n").is_err());
    }
}
