//! How this computer and the online cameras are doing (see
//! [`crate::performance`]).

use dioxus::prelude::*;
use protocol::Memory;

use super::use_watch;
use crate::performance::{CameraUsage, ComputerUsage, LOSS_WINDOW};

/// In degrees Celsius; Raspberry Pis start slowing down at 80.
const HOT: f64 = 80.0;
const GIGABYTE: f64 = 1e9;

#[component]
pub fn Performance() -> Element {
    let performance = use_watch(|hub| &hub.performance);
    let cameras = use_watch(|hub| &hub.cameras);
    let performance = performance.read();
    let online: Vec<_> = cameras
        .read()
        .iter()
        .filter(|(_, status)| status.online)
        .map(|(camera, status)| {
            (
                camera.clone(),
                performance.cameras.get(camera).cloned(),
                status.previews_dropped,
            )
        })
        .collect();
    rsx! {
        fieldset {
            legend { "This computer" }
            match &performance.computer {
                Some(computer) => rsx! { Computer { usage: computer.clone() } },
                None => rsx! { p { class: "muted", "Measuring…" } },
            }
        }
        if online.is_empty() {
            p { class: "muted", "No cameras online." }
        }
        for (camera, usage, previews_dropped) in online {
            fieldset { key: "{camera}",
                legend { "{camera}" }
                match usage {
                    Some(usage) => rsx! { Camera { usage, previews_dropped } },
                    None => rsx! { p { class: "muted", "Waiting for its report…" } },
                }
            }
        }
    }
}

#[component]
fn Computer(usage: ComputerUsage) -> Element {
    rsx! {
        Usage { label: "CPU", title: "Share of all the cores' time", share: usage.cpu }
        MemoryUsage { label: "Memory", memory: usage.memory }
        if usage.gpus.is_empty() {
            p { class: "muted", "No NVIDIA GPU found." }
        }
        for gpu in usage.gpus {
            Usage { label: "GPU", title: "{gpu.name}: share of the time it was busy", share: gpu.busy }
            MemoryUsage { label: "GPU memory", memory: gpu.memory }
        }
    }
}

#[component]
fn Camera(usage: CameraUsage, previews_dropped: Option<f64>) -> Element {
    let window = LOSS_WINDOW.as_secs();
    rsx! {
        match usage.cpu {
            Some(cpu) => rsx! { Usage { label: "CPU", title: "Share of all the cores' time", share: cpu } },
            None => rsx! { Unknown { label: "CPU" } },
        }
        match usage.memory {
            Some(memory) => rsx! { MemoryUsage { label: "Memory", memory } },
            None => rsx! { Unknown { label: "Memory" } },
        }
        match usage.temperature {
            Some(temperature) => rsx! {
                div { class: "row",
                    title: "Of the processor; it slows down from {HOT} °C",
                    span { class: "label", "Temperature" }
                    span { class: if temperature >= HOT { "error" }, "{temperature:.1} °C" }
                }
            },
            None => rsx! { Unknown { label: "Temperature" } },
        }
        if let Some(dropped) = previews_dropped {
            div { class: "row",
                title: "Preview frames this computer dropped in the last {window} seconds because stitching could not keep up",
                span { class: "label", "Stitched previews" }
                span { "Dropped {dropped * 100.0:.1}%" }
            }
        }
        table { class: "values losses",
            tr {
                th { "Lost in {window} s" }
                th { "Left" }
                th { "Right" }
            }
            tr { title: "Frames the camera couldn't record in time, which leave gaps in recordings",
                td { "Frames" }
                for loss in usage.losses {
                    td { {loss_share(loss.dropped)} }
                }
            }
            tr { title: "Previews the camera didn't encode or this computer didn't take in time, which only make the live view stutter",
                td { "Previews" }
                for loss in usage.losses {
                    td { {loss_share(loss.previews_dropped)} }
                }
            }
        }
    }
}

fn loss_share(share: Option<f64>) -> String {
    share.map_or_else(|| "no frames".to_owned(), |share| format!("{:.1}%", share * 100.0))
}

#[component]
fn Usage(label: &'static str, title: String, share: f64) -> Element {
    rsx! {
        div { class: "row usage", title,
            span { class: "label", "{label}" }
            progress { max: 1, value: share }
            span { class: "value", "{share * 100.0:.0}%" }
        }
    }
}

#[component]
fn MemoryUsage(label: &'static str, memory: Memory) -> Element {
    let (used, total) = (memory.used as f64 / GIGABYTE, memory.total as f64 / GIGABYTE);
    rsx! {
        Usage {
            label,
            title: "{used:.1} of {total:.1} GB in use",
            share: if memory.total > 0 { used / total } else { 0.0 },
        }
    }
}

#[component]
fn Unknown(label: &'static str) -> Element {
    rsx! {
        div { class: "row",
            span { class: "label", "{label}" }
            span { class: "muted", "Unknown" }
        }
    }
}
