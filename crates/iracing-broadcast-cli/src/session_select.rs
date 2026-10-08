//! Operator-friendly session and time selection for replay commands.
//!
//! Resolution is data-driven: selectors are validated against the sessions
//! present in the live iRacing session metadata. Nothing is guessed; missing
//! or ambiguous matches are rejected with the available sessions listed.

use anyhow::{Result, anyhow, bail};
use iracing_sdk::schema::session::Session;

/// Parse a human session-time duration into milliseconds.
///
/// Accepted forms are `SS`, `MM:SS`, and `HH:MM:SS`. Seconds in a two- or
/// three-part duration must be `0..=59`; minutes and hours are unbounded
/// apart from the result fitting a `u32` millisecond count.
///
/// ```
/// # use iracing_broadcast_cli::parse_session_time;
/// assert_eq!(parse_session_time("20").unwrap(), 20_000);
/// assert_eq!(parse_session_time("31:20").unwrap(), 1_880_000);
/// assert_eq!(parse_session_time("5:31:20").unwrap(), 19_880_000);
/// ```
///
/// # Errors
///
/// Returns an error when the input is not `SS`, `MM:SS`, or `HH:MM:SS`, when
/// sub-minute components exceed 59, or when the millisecond result overflows
/// `u32`.
pub fn parse_session_time(value: &str) -> Result<u32> {
    let parts: Vec<&str> = value.trim().split(':').collect();
    if parts.len() > 3 {
        bail!(
            "invalid session time {value:?}; expected SS, MM:SS, or HH:MM:SS \
             (e.g. 20, 31:20, 5:31:20)"
        );
    }

    let mut numbers = Vec::with_capacity(parts.len());
    for part in &parts {
        match part.trim().parse::<u32>() {
            Ok(number) => numbers.push(number),
            Err(_) => bail!(
                "invalid session time {value:?}; expected SS, MM:SS, or HH:MM:SS \
                 (e.g. 20, 31:20, 5:31:20)"
            ),
        }
    }

    let seconds: u64 = match numbers[..] {
        [seconds] => seconds.into(),
        [minutes, seconds] if seconds < 60 => u64::from(minutes) * 60 + u64::from(seconds),
        [hours, minutes, seconds] if minutes < 60 && seconds < 60 => {
            u64::from(hours) * 3600 + u64::from(minutes) * 60 + u64::from(seconds)
        }
        _ => bail!(
            "invalid session time {value:?}; sub-minute components must be 0-59 \
             (e.g. 20, 31:20, 5:31:20)"
        ),
    };

    let millis = seconds * 1000;
    u32::try_from(millis).map_err(|_| {
        anyhow!("session time {value:?} exceeds the maximum representable session time")
    })
}

/// Resolve an operator session selector against the live session list.
///
/// The selector is either a numeric iRacing session number (`session_num`)
/// or a human-facing name such as `race`, `practice`, `qualify`, or `heat`.
/// Names are matched case- and punctuation-insensitively against each
/// session's `SessionType` and `SessionName`; exact matches win over
/// substring matches. A numeric selector must exist in the session list.
///
/// # Errors
///
/// Returns an error when the selector is empty, matches no session, or
/// matches more than one session. Error messages list the sessions that are
/// actually available so an operator can recover with a numeric selector.
pub fn resolve_session(selector: &str, sessions: &[Session]) -> Result<i32> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        bail!(
            "session selector is empty; available sessions: {}",
            describe_sessions(sessions)
        );
    }

    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        let wanted: i64 = trimmed.parse().map_err(|_| {
            anyhow!(
                "session number {trimmed} is out of range; available sessions: {}",
                describe_sessions(sessions)
            )
        })?;
        if let Some(session) = sessions
            .iter()
            .find(|session| i64::from(session.session_num) == wanted)
        {
            return Ok(session.session_num);
        }
        bail!(
            "no session {trimmed} in the current session metadata; available sessions: {}",
            describe_sessions(sessions)
        );
    }

    if sessions.is_empty() {
        bail!("the current session metadata contains no sessions to match {trimmed:?} against");
    }

    let wanted = normalize_selector(trimmed);
    if wanted.is_empty() {
        bail!("session selector {trimmed:?} contains no matchable characters");
    }

    let matches = |predicate: &dyn Fn(&str) -> bool| -> Vec<&Session> {
        sessions
            .iter()
            .filter(|session| session_candidates(session).any(|candidate| predicate(&candidate)))
            .collect()
    };

    let exact = matches(&|candidate: &str| candidate == wanted);
    if exact.len() == 1 {
        return Ok(exact[0].session_num);
    }
    if exact.len() > 1 {
        return Err(ambiguous(&wanted, &exact));
    }

    let partial = matches(&|candidate: &str| candidate.contains(&wanted));
    match partial.len() {
        1 => Ok(partial[0].session_num),
        0 => Err(no_match(&wanted, sessions)),
        _ => Err(ambiguous(&wanted, &partial)),
    }
}

/// Fold a selector or session label to a compact match key: lowercase
/// alphanumerics only, so `Warm-up`, `warm up`, and `WARM_UP` all compare
/// equal.
fn compact(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

/// Apply alias folding to the compacted selector (spelling variants that
/// operators use interchangeably).
fn normalize_selector(selector: &str) -> String {
    let compacted = compact(selector);
    match compacted.as_str() {
        "qualifying" | "quals" => "qualify".to_string(),
        "practise" => "practice".to_string(),
        _ => compacted,
    }
}

/// Compact candidate labels (`SessionType`, then `SessionName`) for a session.
fn session_candidates(session: &Session) -> impl Iterator<Item = String> {
    let type_label = compact(&session.session_type);
    let name_label = session.session_name.as_deref().map(compact);
    [Some(type_label), name_label]
        .into_iter()
        .flatten()
        .filter(|label| !label.is_empty())
}

fn describe_session(session: &Session) -> String {
    match &session.session_name {
        Some(name) => format!(
            "{}={} (name {:?})",
            session.session_num, session.session_type, name
        ),
        None => format!("{}={}", session.session_num, session.session_type),
    }
}

fn describe_sessions(sessions: &[Session]) -> String {
    if sessions.is_empty() {
        return "<none>".to_string();
    }
    sessions
        .iter()
        .map(describe_session)
        .collect::<Vec<_>>()
        .join(", ")
}

fn ambiguous(wanted: &str, matched: &[&Session]) -> anyhow::Error {
    let hits = matched
        .iter()
        .map(|session| describe_session(session))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow!(
        "session selector {wanted:?} is ambiguous; it matches [{hits}]; \
         re-run with the numeric session number"
    )
}

fn no_match(wanted: &str, sessions: &[Session]) -> anyhow::Error {
    anyhow!(
        "no session matches {wanted:?}; available sessions: {}",
        describe_sessions(sessions)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(num: i32, kind: &str, name: Option<&str>) -> Session {
        Session {
            session_num: num,
            session_type: kind.to_string(),
            session_name: name.map(str::to_string),
            ..Session::default()
        }
    }

    fn weekend() -> Vec<Session> {
        vec![
            session(0, "Practice", Some("Practice")),
            session(1, "Qualify", Some("Qualify")),
            session(2, "Race", Some("Race")),
        ]
    }

    #[test]
    fn duration_forms_convert_to_milliseconds() {
        assert_eq!(parse_session_time("20").unwrap(), 20_000);
        assert_eq!(parse_session_time("31:20").unwrap(), 1_880_000);
        assert_eq!(parse_session_time("5:31:20").unwrap(), 19_880_000);
        assert_eq!(parse_session_time(" 7 ").unwrap(), 7_000);
        assert_eq!(parse_session_time("0").unwrap(), 0);
        assert_eq!(parse_session_time("90:20").unwrap(), 5_420_000);
        assert_eq!(parse_session_time("23:59:59").unwrap(), 86_399_000);
    }

    #[test]
    fn single_seconds_component_is_not_capped() {
        assert_eq!(parse_session_time("700").unwrap(), 700_000);
    }

    #[test]
    fn malformed_durations_are_rejected() {
        for bad in ["", " ", "abc", "-5", "20.5", ":", "5:", ":20", "5:31:20:0"] {
            assert!(
                parse_session_time(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn sub_minute_components_must_fit_their_range() {
        for bad in ["5:70", "1:60:15", "1:15:60", "31:59:60"] {
            assert!(
                parse_session_time(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn overflowed_millisecond_results_are_rejected() {
        assert!(parse_session_time("4294968").is_err());
        assert!(parse_session_time("1300000:00").is_err());
    }

    #[test]
    fn numeric_selector_resolves_matching_session() {
        assert_eq!(resolve_session("1", &weekend()).unwrap(), 1);
        assert_eq!(resolve_session(" 2 ", &weekend()).unwrap(), 2);
    }

    #[test]
    fn missing_numeric_selector_lists_available_sessions() {
        let error = resolve_session("7", &weekend()).unwrap_err().to_string();
        assert!(error.contains("no session 7"), "{error}");
        for fragment in ["0=Practice", "1=Qualify", "2=Race"] {
            assert!(error.contains(fragment), "{error} missing {fragment}");
        }
    }

    #[test]
    fn human_selector_resolves_case_insensitively() {
        assert_eq!(resolve_session("race", &weekend()).unwrap(), 2);
        assert_eq!(resolve_session("Race", &weekend()).unwrap(), 2);
        assert_eq!(resolve_session("PRACTICE", &weekend()).unwrap(), 0);
        assert_eq!(resolve_session("qualify", &weekend()).unwrap(), 1);
    }

    #[test]
    fn selector_aliases_resolve_to_canonical_types() {
        assert_eq!(resolve_session("qualifying", &weekend()).unwrap(), 1);
        assert_eq!(resolve_session("quals", &weekend()).unwrap(), 1);
        assert_eq!(resolve_session("practise", &weekend()).unwrap(), 0);
    }

    #[test]
    fn punctuation_and_spacing_are_folded() {
        let sessions = vec![
            session(3, "Open Practice", None),
            session(4, "Warm-up", None),
        ];
        assert_eq!(resolve_session("open-practice", &sessions).unwrap(), 3);
        assert_eq!(resolve_session("warm up", &sessions).unwrap(), 4);
        assert_eq!(resolve_session("warmup", &sessions).unwrap(), 4);
    }

    #[test]
    fn substring_matches_apply_when_no_exact_match_exists() {
        let sessions = vec![session(0, "Open Practice", None), session(2, "Race", None)];
        assert_eq!(resolve_session("practice", &sessions).unwrap(), 0);
    }

    #[test]
    fn exact_match_wins_over_substring_match() {
        let sessions = vec![session(1, "Sprint Race", None), session(2, "Race", None)];
        assert_eq!(resolve_session("race", &sessions).unwrap(), 2);
    }

    #[test]
    fn session_name_selectors_resolve() {
        let sessions = vec![
            session(0, "Race", Some("Heat 1")),
            session(1, "Race", Some("Heat 2")),
        ];
        assert_eq!(resolve_session("heat 1", &sessions).unwrap(), 0);
        assert_eq!(resolve_session("Heat 2", &sessions).unwrap(), 1);
    }

    #[test]
    fn ambiguous_selector_requires_numeric_disambiguation() {
        let sessions = vec![
            session(0, "Race", Some("Heat 1")),
            session(1, "Race", Some("Heat 2")),
        ];
        let error = resolve_session("race", &sessions).unwrap_err().to_string();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("0=Race"), "{error}");
        assert!(error.contains("1=Race"), "{error}");
    }

    #[test]
    fn ambiguous_substring_selector_is_rejected() {
        let sessions = vec![
            session(0, "Open Practice", None),
            session(1, "Qualify", Some("Practice Session")),
        ];
        let error = resolve_session("practice", &sessions)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn unmatched_selector_lists_sessions() {
        let error = resolve_session("heat", &weekend()).unwrap_err().to_string();
        assert!(error.contains("no session matches"), "{error}");
        assert!(error.contains("0=Practice"), "{error}");
    }

    #[test]
    fn empty_selector_and_empty_metadata_are_rejected() {
        assert!(resolve_session("", &weekend()).is_err());
        assert!(resolve_session("   ", &weekend()).is_err());
        assert!(resolve_session("race", &[]).is_err());
        assert!(resolve_session("!!!", &weekend()).is_err());
    }
}
