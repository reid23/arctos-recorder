use dioxus::prelude::*;
use stitching::JobStatus;

use super::{use_hub, use_watch};
use crate::jobs::Job;

#[component]
pub fn Jobs() -> Element {
    let jobs = use_watch(|hub| &hub.jobs);
    rsx! {
        if jobs.read().is_empty() {
            p { class: "empty", "Match recordings appear here once a camera uploads them." }
        } else {
            table { class: "jobs",
                thead {
                    tr {
                        th { "Match" }
                        th { "Camera" }
                        th { "Status" }
                        th {}
                    }
                }
                tbody {
                    for job in jobs.read().iter().rev().cloned() {
                        JobRow { key: "{job.key.match_id}/{job.key.camera}", job }
                    }
                }
            }
        }
    }
}

#[component]
fn JobRow(job: Job) -> Element {
    let hub = use_hub();
    let (label, progress) = match &job.status {
        JobStatus::Queued => ("Queued".to_owned(), None),
        JobStatus::Sending(p) => ("Sending to cloud".to_owned(), Some(*p)),
        JobStatus::Stitching(p) => ("Stitching".to_owned(), Some(*p)),
        JobStatus::Uploading(p) => ("Uploading".to_owned(), Some(*p)),
        JobStatus::Done => ("Uploaded".to_owned(), None),
        JobStatus::Failed(error) => (format!("Failed: {error}"), None),
    };
    let failed = matches!(job.status, JobStatus::Failed(_));
    let retry = {
        let (key, title) = (job.key.clone(), job.title.clone());
        move |_| hub.enqueue(key.clone(), title.clone())
    };
    rsx! {
        tr {
            td { "{job.title}" }
            td { "{job.key.camera}" }
            td { class: if failed { "error" },
                "{label}"
                if let Some(p) = progress {
                    progress { max: 1.0, value: p }
                }
            }
            td {
                if failed {
                    button { onclick: retry, "Retry" }
                }
            }
        }
    }
}
