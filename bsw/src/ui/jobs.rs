use dioxus::prelude::*;
use stitching::JobStatus;

use super::{use_hub, use_watch};
use crate::hub::Hub;
use crate::jobs::Job;
use crate::settings::Stitching;

#[component]
pub fn Jobs() -> Element {
    let hub = use_hub();
    let jobs = use_watch(|hub| &hub.jobs);
    let settings = use_watch(|hub| &hub.settings);
    let waiting = |status: JobStatus| {
        jobs.read()
            .iter()
            .filter(|job| job.status == status)
            .count()
    };
    let (stitch_waiting, upload_waiting) = (
        waiting(JobStatus::StitchLater),
        waiting(JobStatus::UploadLater),
    );
    let stitch_all = match settings.read().stitching {
        Stitching::Local => "Stitch all",
        Stitching::Cloud => "Send all to the cloud",
    };
    rsx! {
        div { class: "toolbar",
            button {
                disabled: stitch_waiting == 0,
                onclick: {
                    let hub = hub.clone();
                    move |_| release_all(&hub, JobStatus::StitchLater)
                },
                "{stitch_all} ({stitch_waiting})"
            }
            button {
                disabled: upload_waiting == 0,
                onclick: move |_| release_all(&hub, JobStatus::UploadLater),
                "Upload all ({upload_waiting})"
            }
        }
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

/// Lets every job waiting with `status` go ahead.
fn release_all(hub: &Hub, status: JobStatus) {
    let waiting: Vec<Job> = hub
        .jobs
        .borrow()
        .iter()
        .filter(|job| job.status == status)
        .cloned()
        .collect();
    for job in waiting {
        hub.enqueue(job.key, job.title, true);
    }
}

#[component]
fn JobRow(job: Job) -> Element {
    let hub = use_hub();
    let settings = use_watch(|hub| &hub.settings);
    let (label, progress) = match &job.status {
        JobStatus::Receiving(p) => ("Receiving from camera".to_owned(), Some(*p)),
        JobStatus::Queued => ("Queued".to_owned(), None),
        JobStatus::StitchLater => ("Waiting to be stitched".to_owned(), None),
        JobStatus::UploadLater => ("Stitched; waiting to be uploaded".to_owned(), None),
        JobStatus::Sending(p) => ("Sending to cloud".to_owned(), Some(*p)),
        JobStatus::Stitching(p) => ("Stitching".to_owned(), Some(*p)),
        JobStatus::Uploading(p) => ("Uploading".to_owned(), Some(*p)),
        JobStatus::Done => ("Uploaded".to_owned(), None),
        JobStatus::Failed(error) => (format!("Failed: {error}"), None),
    };
    let action = match job.status {
        JobStatus::Failed(_) => Some("Retry"),
        JobStatus::StitchLater => Some(match settings.read().stitching {
            Stitching::Local => "Stitch now",
            Stitching::Cloud => "Send now",
        }),
        JobStatus::UploadLater => Some("Upload now"),
        _ => None,
    };
    let failed = matches!(job.status, JobStatus::Failed(_));
    let release = {
        let (key, title) = (job.key.clone(), job.title.clone());
        move |_| hub.enqueue(key.clone(), title.clone(), true)
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
                if let Some(action) = action {
                    button { onclick: release, "{action}" }
                }
            }
        }
    }
}
