//! Polls the Arctos scoreboard of every field that has a camera and turns it
//! into recording directives.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use protocol::Directive;
use tokio::time::MissedTickBehavior;

use arctos::Scoreboard;

use crate::hub::{FieldStatus, Hub};

const POLL: Duration = Duration::from_secs(1);
const FIELD_LIST_POLL: Duration = Duration::from_secs(30);

pub async fn run(hub: Arc<Hub>) {
    tokio::spawn(poll_field_list(hub.clone()));

    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let settings = hub.settings.borrow().clone();
        let fields: BTreeSet<&str> = settings.fields().collect();
        let scoreboards = join_all(fields.iter().map(|field| {
            hub.arctos
                .scoreboard(&settings.arctos.tournament, field, hub.account.as_ref())
        }))
        .await;

        let previous = hub.fields.borrow().clone();
        let mut statuses = BTreeMap::new();
        for (field, scoreboard) in fields.into_iter().zip(scoreboards) {
            let status = match scoreboard {
                Ok(scoreboard) => {
                    let match_title = scoreboard.as_ref().map(|scoreboard| {
                        let title = scoreboard.title();
                        hub.remember_match_title(&scoreboard.id, title.clone());
                        title
                    });
                    FieldStatus {
                        directive: directive(scoreboard.as_ref()),
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

fn directive(scoreboard: Option<&Scoreboard>) -> Directive {
    match scoreboard {
        None => Directive::Idle,
        Some(scoreboard) => Directive::Record {
            match_id: scoreboard.id.clone(),
            play: scoreboard.points.iter().map(|point| point.span()).collect(),
        },
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
            .map_err(|e| format!("{e:#}"));
        hub.arctos_fields.send_modify(|f| *f = fields);
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
