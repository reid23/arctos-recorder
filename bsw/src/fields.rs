//! Polls the Arctos scoreboard of every field that has a camera and turns it
//! into recording directives.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures_util::future::join_all;
use protocol::{Directive, Span, Timestamp};
use tokio::time::MissedTickBehavior;

use arctos::{Scoreboard, timestamp};

use crate::hub::{FieldStatus, Hub};

const POLL: Duration = Duration::from_secs(1);
const FIELD_LIST_POLL: Duration = Duration::from_secs(30);

pub async fn run(hub: Arc<Hub>) {
    tokio::spawn(poll_field_list(hub.clone()));

    // When each match without per-point times was first seen in progress.
    let mut first_seen: HashMap<String, Timestamp> = HashMap::new();
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let settings = hub.settings.borrow().clone();
        let fields: BTreeSet<&str> = settings.fields().collect();
        let scoreboards = join_all(
            fields
                .iter()
                .map(|field| hub.arctos.scoreboard(&settings.arctos.tournament, field)),
        )
        .await;

        let previous = hub.fields.borrow().clone();
        let mut statuses = BTreeMap::new();
        for (field, scoreboard) in fields.into_iter().zip(scoreboards) {
            let status = match scoreboard {
                Ok(scoreboard) => {
                    let match_title = scoreboard.match_id.as_ref().map(|id| {
                        let title = scoreboard.title();
                        hub.remember_match_title(id, title.clone());
                        title
                    });
                    FieldStatus {
                        directive: directive(&scoreboard, &mut first_seen),
                        match_title,
                        error: None,
                    }
                }
                Err(e) => FieldStatus {
                    error: Some(format!("{e:#}")),
                    ..previous.get(field).cloned().unwrap_or(FieldStatus {
                        directive: Directive::Idle,
                        match_title: None,
                        error: None,
                    })
                },
            };
            statuses.insert(field.to_owned(), status);
        }
        hub.fields.send_if_modified(|current| {
            let changed = *current != statuses;
            *current = statuses;
            changed
        });
    }
}

fn directive(scoreboard: &Scoreboard, first_seen: &mut HashMap<String, Timestamp>) -> Directive {
    let Some(match_id) = scoreboard
        .match_id
        .clone()
        .filter(|_| scoreboard.has_active_match)
    else {
        return Directive::Idle;
    };
    let play = match &scoreboard.points_for_stones {
        Some(points) => points.iter().map(|point| point.span()).collect(),
        // Arctos only reports point times for stones matches, so other
        // matches are recorded in full.
        None => {
            let start = *first_seen
                .entry(match_id.clone())
                .or_insert_with(|| timestamp(Utc::now()));
            vec![Span { start, end: None }]
        }
    };
    Directive::Record { match_id, play }
}

async fn poll_field_list(hub: Arc<Hub>) {
    let mut settings = hub.settings.subscribe();
    loop {
        let arctos = settings.borrow_and_update().arctos.clone();
        let fields = hub
            .arctos
            .fields(&arctos.tournament, &arctos.credentials)
            .await
            .map_err(|e| format!("{e:#}"));
        hub.arctos_fields.send_modify(|f| *f = fields);
        // Refresh when the Arctos settings change, or periodically.
        let changed = async {
            while settings.changed().await.is_ok() {
                if settings.borrow_and_update().arctos != arctos {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(FIELD_LIST_POLL, changed).await;
    }
}
