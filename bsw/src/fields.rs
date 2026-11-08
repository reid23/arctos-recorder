//! Polls the Arctos scoreboard of every field that has a camera and turns it
//! into recording directives.
//!
//! A match ends only when its stored status is completed or skipped, when a
//! different match is in progress on the field, or when the operator finishes
//! the recording. A failed poll keeps the last directive, and a match already
//! left is not started again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::future::join_all;
use protocol::{Directive, Span, Timestamp};
use tokio::time::MissedTickBehavior;

use arctos::{MatchProgress, ScoreboardPoint, timestamp};

use crate::hub::{FieldStatus, Hub};

const POLL: Duration = Duration::from_secs(1);
const FIELD_LIST_POLL: Duration = Duration::from_secs(30);

pub async fn run(hub: Arc<Hub>) {
    tokio::spawn(poll_field_list(hub.clone()));

    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut memories = BTreeMap::<String, Memory>::new();
    let mut offset_micros = 0_i64;
    loop {
        ticker.tick().await;
        let settings = hub.settings.borrow().clone();
        let fields: BTreeSet<String> = settings.fields().map(str::to_owned).collect();
        let finish = hub.take_finish_requests();
        let scoreboards = join_all(fields.iter().map(|field| {
            hub.arctos
                .scoreboard(&settings.arctos.tournament, field, hub.account.as_ref())
        }))
        .await;

        let mut statuses = BTreeMap::new();
        for (field, scoreboard) in fields.iter().zip(scoreboards) {
            let open_id = memories
                .get(field)
                .and_then(|memory| memory.open.as_ref().map(|open| open.id.clone()));
            let operator_finished = open_id.as_ref().is_some_and(|id| finish.contains(id));
            let report = match scoreboard {
                Err(error) => Report::Unreachable {
                    error: format!("{error:#}"),
                },
                Ok(timed) => {
                    if let Some(offset) = timed.offset {
                        offset_micros = offset;
                    }
                    match timed.value {
                        Some(board) => {
                            let title = board.title();
                            hub.remember_match_title(&board.id, title.clone());
                            Report::Live(Seen {
                                id: board.id,
                                play: spans(&board.points, offset_micros),
                                title,
                            })
                        }
                        None => {
                            let progress = match &open_id {
                                Some(match_id) if !operator_finished => {
                                    Some(progress_of(&hub, &settings.arctos.tournament, match_id, &mut offset_micros).await)
                                }
                                _ => None,
                            };
                            Report::NoMatch { progress }
                        }
                    }
                }
            };
            let memory = memories.entry(field.clone()).or_default();
            let was_open = memory.open.as_ref().map(|open| open.id.clone());
            let was_closing = memory.closing.as_ref().map(|closing| closing.id.clone());
            let status = step(memory, timestamp(Utc::now()), operator_finished, report);
            if memory.open.as_ref().map(|open| &open.id) != was_open.as_ref()
                && let Some(open) = &memory.open
            {
                tracing::info!("recording {} on {field}", open.id);
            }
            if memory.closing.as_ref().map(|closing| &closing.id) != was_closing.as_ref()
                && let Some(closing) = &memory.closing
            {
                tracing::info!("finishing the recording of {} on {field}", closing.id);
            }
            statuses.insert(field.clone(), status);
        }
        hub.fields.send_if_modified(|current| {
            let changed = *current != statuses;
            *current = statuses;
            changed
        });
    }
}

/// The match being recorded, or the one whose end is still being repeated
/// for a camera that may have missed it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MatchRecord {
    id: String,
    play: Vec<Span>,
    title: String,
    confirmed_at: Timestamp,
}

#[derive(Default)]
struct Memory {
    open: Option<MatchRecord>,
    /// Published until a newer match starts, so a lost poll still ends the recording.
    closing: Option<MatchRecord>,
    /// Matches this field has already left. A scoreboard that shows one of
    /// them does not become the active match.
    left: BTreeSet<String>,
}

struct Seen {
    id: String,
    play: Vec<Span>,
    title: String,
}

enum Report {
    Unreachable { error: String },
    Live(Seen),
    /// The scoreboard has no match in progress. `progress` is the open
    /// match's own record, when there is one and this poll could read it.
    NoMatch { progress: Option<Progress> },
}

enum Progress {
    Ended(Vec<Span>),
    StillGoing,
    Unreachable { error: String },
}

fn step(memory: &mut Memory, now: Timestamp, operator_finished: bool, report: Report) -> FieldStatus {
    if operator_finished && !replacing_with_a_new_match(memory, &report)
        && let Some(open) = memory.open.take()
    {
        finish(memory, open, now);
    }
    match report {
        Report::Unreachable { error } => view(memory, Some(error)),
        Report::Live(seen) => {
            if memory.left.contains(&seen.id) {
                return view(memory, None);
            }
            if memory.open.as_ref().is_some_and(|open| open.id != seen.id) {
                let previous = memory.open.take().expect("checked");
                memory.left.insert(previous.id);
            }
            memory.closing = None;
            memory.open = Some(MatchRecord {
                id: seen.id,
                play: seen.play,
                title: seen.title,
                confirmed_at: now,
            });
            view(memory, None)
        }
        Report::NoMatch { progress } => {
            if memory.open.is_none() {
                return view(memory, None);
            }
            match progress {
                Some(Progress::Ended(play)) => {
                    let mut open = memory.open.take().expect("checked");
                    open.play = play;
                    finish(memory, open, now);
                    view(memory, None)
                }
                Some(Progress::Unreachable { error }) => view(memory, Some(error)),
                Some(Progress::StillGoing) | None => view(memory, None),
            }
        }
    }
}

/// A different match, not one already left, is on the scoreboard. Starting
/// it is what ends the open recording, so the operator's finish is redundant.
fn replacing_with_a_new_match(memory: &Memory, report: &Report) -> bool {
    let Report::Live(seen) = report else {
        return false;
    };
    memory.open.as_ref().is_some_and(|open| open.id != seen.id) && !memory.left.contains(&seen.id)
}

fn finish(memory: &mut Memory, open: MatchRecord, now: Timestamp) {
    memory.left.insert(open.id.clone());
    memory.closing = Some(MatchRecord {
        confirmed_at: now,
        ..open
    });
}

fn view(memory: &Memory, error: Option<String>) -> FieldStatus {
    if let Some(open) = &memory.open {
        FieldStatus {
            directive: Directive::Record {
                match_id: open.id.clone(),
                play: open.play.clone(),
            },
            confirmed_at: open.confirmed_at,
            match_title: Some(open.title.clone()),
            error,
        }
    } else if let Some(closing) = &memory.closing {
        FieldStatus {
            directive: Directive::Finalize {
                match_id: closing.id.clone(),
                play: closing.play.clone(),
            },
            confirmed_at: closing.confirmed_at,
            match_title: Some(closing.title.clone()),
            error,
        }
    } else {
        FieldStatus {
            directive: Directive::Idle,
            confirmed_at: Timestamp(0),
            match_title: None,
            error,
        }
    }
}

async fn progress_of(hub: &Hub, tournament: &str, match_id: &str, offset_micros: &mut i64) -> Progress {
    match hub
        .arctos
        .match_progress(tournament, match_id, hub.account.as_ref())
        .await
    {
        Ok(timed) => {
            if let Some(offset) = timed.offset {
                *offset_micros = offset;
            }
            if ended(&timed.value) {
                Progress::Ended(spans(&timed.value.points, *offset_micros))
            } else {
                Progress::StillGoing
            }
        }
        Err(error) => Progress::Unreachable {
            error: format!("{error:#}"),
        },
    }
}

/// Whether the match record is a positive end. A static break's reported
/// status follows the clock, not a referee ending it, and a field match is
/// not one.
fn ended(progress: &MatchProgress) -> bool {
    progress.schedule_type != "STATBREAK" && matches!(progress.status.as_str(), "COMPLETED" | "SKIPPED")
}

fn spans(points: &[ScoreboardPoint], offset_micros: i64) -> Vec<Span> {
    points
        .iter()
        .map(|point| shift(point.span(), offset_micros))
        .collect()
}

/// `offset_micros` is Arctos's clock minus the base station's.
fn shift(span: Span, offset_micros: i64) -> Span {
    Span {
        start: Timestamp(span.start.0 - offset_micros),
        end: span.end.map(|end| Timestamp(end.0 - offset_micros)),
    }
}

async fn poll_field_list(hub: Arc<Hub>) {
    let mut settings = hub.settings.subscribe();
    let mut account = hub.account.status.subscribe();
    loop {
        let arctos = settings.borrow_and_update().arctos.clone();
        account.mark_unchanged();
        let fields = hub
            .arctos
            .fields(&arctos.tournament, hub.account.as_ref())
            .await
            .map_err(|error| format!("{error:#}"));
        hub.arctos_fields.send_modify(|fields_slot| *fields_slot = fields);
        // Refresh when the Arctos settings or the account change, or
        // periodically.
        let changed = async {
            loop {
                tokio::select! {
                    _ = settings.changed() => {
                        if settings.borrow_and_update().arctos != arctos {
                            return;
                        }
                    }
                    _ = account.changed() => return,
                }
            }
        };
        let _ = tokio::time::timeout(FIELD_LIST_POLL, changed).await;
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    fn at(micros: i64) -> Timestamp {
        Timestamp(micros)
    }

    fn open(memory: &mut Memory, id: &str, confirmed_at: Timestamp) {
        memory.open = Some(MatchRecord {
            id: id.to_owned(),
            play: vec![],
            title: id.to_owned(),
            confirmed_at,
        });
    }

    fn live(id: &str) -> Report {
        Report::Live(Seen {
            id: id.to_owned(),
            play: vec![],
            title: id.to_owned(),
        })
    }

    fn record_id(status: &FieldStatus) -> Option<&str> {
        match &status.directive {
            Directive::Record { match_id, .. } | Directive::Finalize { match_id, .. } => Some(match_id),
            Directive::Idle => None,
        }
    }

    #[test]
    fn a_failed_poll_keeps_the_open_match() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let status = step(
            &mut memory,
            at(99),
            false,
            Report::Unreachable {
                error: "down".to_owned(),
            },
        );
        assert_eq!(record_id(&status), Some("a"));
        assert_eq!(status.confirmed_at, at(10));
        assert_eq!(status.error.as_deref(), Some("down"));
        assert!(matches!(status.directive, Directive::Record { .. }));
    }

    #[test]
    fn an_empty_scoreboard_does_not_end_a_match_that_is_still_going() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let status = step(
            &mut memory,
            at(99),
            false,
            Report::NoMatch {
                progress: Some(Progress::StillGoing),
            },
        );
        assert!(matches!(status.directive, Directive::Record { .. }));
        assert_eq!(status.confirmed_at, at(10));
        assert!(status.error.is_none());
    }

    #[test]
    fn a_completed_match_is_finalized_and_not_reopened() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let play = vec![Span {
            start: at(1),
            end: Some(at(2)),
        }];
        let status = step(
            &mut memory,
            at(40),
            false,
            Report::NoMatch {
                progress: Some(Progress::Ended(play.clone())),
            },
        );
        assert_eq!(
            status.directive,
            Directive::Finalize {
                match_id: "a".to_owned(),
                play: play.clone(),
            }
        );
        assert_eq!(status.confirmed_at, at(40));

        let status = step(&mut memory, at(50), false, live("a"));
        assert_eq!(
            status.directive,
            Directive::Finalize {
                match_id: "a".to_owned(),
                play,
            }
        );
        assert_eq!(status.confirmed_at, at(40));
    }

    #[test]
    fn a_new_match_replaces_the_open_one_and_an_old_id_does_not() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let status = step(&mut memory, at(20), false, live("b"));
        assert_eq!(record_id(&status), Some("b"));
        assert_eq!(status.confirmed_at, at(20));
        assert!(matches!(status.directive, Directive::Record { .. }));

        let status = step(&mut memory, at(30), false, live("a"));
        assert_eq!(record_id(&status), Some("b"));
        assert_eq!(status.confirmed_at, at(20));
        assert!(memory.left.contains("a"));
    }

    #[test]
    fn the_operator_can_finish_a_match_arctos_still_shows() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let status = step(&mut memory, at(50), true, live("a"));
        assert!(matches!(status.directive, Directive::Finalize { .. }));
        assert_eq!(status.confirmed_at, at(50));

        let status = step(&mut memory, at(60), false, live("a"));
        assert!(matches!(status.directive, Directive::Finalize { .. }));
        assert_eq!(status.confirmed_at, at(50));
    }

    #[test]
    fn the_operator_finishing_does_not_block_a_new_match() {
        let mut memory = Memory::default();
        open(&mut memory, "a", at(10));
        let status = step(&mut memory, at(20), true, live("b"));
        assert_eq!(record_id(&status), Some("b"));
        assert!(matches!(status.directive, Directive::Record { .. }));
        assert!(memory.left.contains("a"));
    }

    #[test]
    fn no_match_and_no_recording_is_idle() {
        let mut memory = Memory::default();
        let status = step(&mut memory, at(1), false, Report::NoMatch { progress: None });
        assert_eq!(status.directive, Directive::Idle);
    }

    #[test]
    fn a_static_break_looking_completed_does_not_end_the_recording() {
        let progress = |status: &str, schedule_type: &str| MatchProgress {
            status: status.to_owned(),
            schedule_type: schedule_type.to_owned(),
            points: vec![],
        };
        assert!(!ended(&progress("COMPLETED", "STATBREAK")));
        assert!(ended(&progress("COMPLETED", "STATIC")));
        assert!(ended(&progress("SKIPPED", "SAFE")));
        assert!(!ended(&progress("IN_PROGRESS", "STATIC")));
    }

    #[test]
    fn point_times_are_shifted_into_the_base_station_clock() {
        let point = ScoreboardPoint {
            stamp: DateTime::from_timestamp(100, 0).unwrap(),
            end_stamp: Some(DateTime::from_timestamp(110, 0).unwrap()),
        };
        let shifted = shift(point.span(), 2_000_000);
        assert_eq!(shifted.start, Timestamp(98_000_000));
        assert_eq!(shifted.end, Some(Timestamp(108_000_000)));
    }
}
