//! Settings take effect as soon as they are changed.

use std::time::Duration;

use dioxus::prelude::*;
use protocol::rig;
use stitching::encode::VideoQuality;
use stitching::gpu::GpuChoice;

use super::controls::NumberSlider;
use super::{use_hub, use_runtime, use_watch};
use crate::account::AccountStatus;
use crate::camera_port;
use crate::settings::{
    Align, LiveSettings, Overlay, RecordingSettings, Settings as AllSettings, Stitching, Workflow,
};

#[component]
pub fn Settings() -> Element {
    let hub = use_hub();
    let settings = use_watch(|hub| &hub.settings);
    let fields = use_watch(|hub| &hub.arctos_fields);
    let settings = settings.read();
    let update = {
        let hub = hub.clone();
        move |change: fn(&mut AllSettings, String)| {
            let hub = hub.clone();
            move |value: String| hub.update_settings(|s| change(s, value))
        }
    };
    let toggle = {
        let hub = hub.clone();
        move |change: fn(&mut Workflow, bool)| {
            let hub = hub.clone();
            move |on: bool| hub.update_settings(|s| change(&mut s.workflow, on))
        }
    };
    let overlay = {
        let hub = hub.clone();
        move |change: fn(&mut Overlay, f64)| {
            let hub = hub.clone();
            move |value: f64| hub.update_settings(|s| change(&mut s.overlay, value))
        }
    };
    let recording_time = {
        let hub = hub.clone();
        move |change: fn(&mut RecordingSettings, Duration)| {
            let hub = hub.clone();
            move |seconds: f64| {
                hub.update_settings(|s| change(&mut s.recording, Duration::from_secs_f64(seconds)))
            }
        }
    };

    rsx! {
        fieldset {
            legend { "Arctos" }
            Account {}
            TextInput {
                label: "Tournament",
                title: "The tournament's name in its address on Arctos: for events.californiajugger.org/spring-open, \
                        spring-open",
                value: settings.arctos.tournament.clone(),
                onchange: update(|s, v| s.arctos.tournament = v),
            }
            match &*fields.read() {
                Ok(fields) if fields.is_empty() => rsx! { p { class: "muted", "No fields found." } },
                Ok(fields) => rsx! { p { class: "muted", "Fields: {fields.join(\", \")}" } },
                Err(error) => rsx! { p { class: "error", "Can't load the tournament's fields: {error}" } },
            }
        }
        if cfg!(target_os = "linux") {
            CameraNetwork {}
        }
        fieldset {
            legend { "Stitching" }
            div { class: "row",
                title: "Where recordings and live streams are stitched: on this computer's GPU, or on a cloud \
                        stitching server, which then also uploads the videos and restreams the live streams",
                for (mode, label) in [(Stitching::Local, "On this computer"), (Stitching::Cloud, "In the cloud")] {
                    label { class: "radio",
                        input {
                            r#type: "radio",
                            name: "stitching",
                            checked: settings.stitching == mode,
                            onchange: {
                                let hub = hub.clone();
                                move |_| hub.update_settings(|s| s.stitching = mode)
                            },
                        }
                        "{label}"
                    }
                }
            }
            if settings.stitching == Stitching::Local {
                div { class: "row",
                    title: "The graphics processor that stitches live previews and recordings on this computer",
                    "GPU "
                    for (choice, label) in [(GpuChoice::Discrete, "Discrete"), (GpuChoice::Integrated, "Integrated")] {
                        label { class: "radio",
                            input {
                                r#type: "radio",
                                name: "gpu",
                                checked: settings.gpu == choice,
                                onchange: {
                                    let hub = hub.clone();
                                    move |_| hub.update_settings(|s| s.gpu = choice)
                                },
                            }
                            "{label}"
                        }
                    }
                }
            }
            if settings.stitching == Stitching::Cloud {
                TextInput {
                    label: "Server URL",
                    title: "The cloud stitching server's address, port included",
                    placeholder: "http://stitcher.example.org:7800",
                    value: settings.cloud.url.clone(),
                    onchange: update(|s, v| s.cloud.url = v),
                }
                TextInput {
                    label: "Token",
                    title: "The secret the cloud stitching server was started with (its SSW_TOKEN)",
                    kind: "password",
                    value: settings.cloud.token.clone(),
                    onchange: update(|s, v| s.cloud.token = v),
                }
            }
        }
        fieldset {
            legend { "Resource Usage" }
            WorkflowCheck {
                label: "Live stream",
                title: "Restream each camera's stitched view to the stream server set in its box",
                checked: settings.workflow.live_stream,
                onchange: toggle(|w, on| w.live_stream = on),
            }
            match settings.stitching {
                Stitching::Local => rsx! {
                    WorkflowCheck {
                        label: "Stitch later",
                        title: "Keep recordings here until you stitch them from the Jobs pane",
                        checked: settings.workflow.stitch_later,
                        onchange: toggle(|w, on| w.stitch_later = on),
                    }
                    WorkflowCheck {
                        label: "Upload later",
                        title: "Keep stitched videos until you upload them from the Jobs pane",
                        checked: settings.workflow.upload_later,
                        onchange: toggle(|w, on| w.upload_later = on),
                    }
                },
                // The stitching server uploads what it stitches.
                Stitching::Cloud => rsx! {
                    WorkflowCheck {
                        label: "Stitch and upload later",
                        title: "Keep recordings here until you send them to the cloud from the Jobs pane",
                        checked: settings.stitches_later(),
                        onchange: toggle(|w, on| {
                            w.stitch_later = on;
                            w.upload_later = on;
                        }),
                    }
                },
            }
        }
        if settings.workflow.live_stream {
            fieldset {
                legend { "Live Stream" }
                NumberInput {
                    label: "Width",
                    title: "Of the stitched live previews and streams; their height follows from each camera's shape",
                    min: f64::from(*LiveSettings::WIDTHS.start()),
                    max: f64::from(*LiveSettings::WIDTHS.end()),
                    step: 8.0,
                    value: f64::from(settings.live.width),
                    unit: "px",
                    onchange: {
                        let hub = hub.clone();
                        move |width: f64| hub.update_settings(|s| s.live.width = (width / 8.0).round() as u32 * 8)
                    },
                }
                NumberInput {
                    label: "Bitrate",
                    title: "Of the streams' video. Higher looks better but needs a faster upload",
                    min: *LiveSettings::BITRATES.start(),
                    max: *LiveSettings::BITRATES.end(),
                    step: 0.5,
                    value: settings.live.bitrate,
                    unit: "Mbps",
                    onchange: {
                        let hub = hub.clone();
                        move |bitrate: f64| hub.update_settings(|s| s.live.bitrate = bitrate)
                    },
                }
                label { class: "row",
                    title: "Of the previews the cameras send, which the streams are stitched from. Bigger looks sharper \
                            but takes more of the cameras' CPU and of the network. A camera recording a match waits \
                            for it to end before changing",
                    span { class: "label", "Camera frames" }
                    select {
                        onchange: {
                            let hub = hub.clone();
                            move |e: FormEvent| {
                                if let Ok(index) = e.value().parse::<usize>() {
                                    hub.update_settings(|s| s.live.camera_size = rig::PREVIEW_SIZES[index]);
                                }
                            }
                        },
                        for (index, (width, height)) in rig::PREVIEW_SIZES.into_iter().enumerate() {
                            option {
                                value: "{index}",
                                selected: (width, height) == settings.live.camera_size,
                                "{width} by {height}"
                            }
                        }
                    }
                }
                NumberInput {
                    label: "Camera JPEG quality",
                    title: "How finely the cameras compress their previews, out of 100. Higher looks sharper but takes \
                            more of the cameras' CPU and of the network",
                    min: f64::from(*LiveSettings::CAMERA_QUALITIES.start()),
                    max: f64::from(*LiveSettings::CAMERA_QUALITIES.end()),
                    step: 1.0,
                    value: f64::from(settings.live.camera_quality),
                    onchange: {
                        let hub = hub.clone();
                        move |quality: f64| hub.update_settings(|s| s.live.camera_quality = quality.round() as u8)
                    },
                }
            }
        }
        fieldset {
            legend { "Recordings" }
            NumberInput {
                label: "JPEG quality",
                title: "How finely the cameras compress the frames they record, out of 100. Higher keeps more detail \
                        but takes more of the cameras' CPU, so they may drop frames, and makes bigger uploads",
                min: f64::from(*RecordingSettings::CAMERA_QUALITIES.start()),
                max: f64::from(*RecordingSettings::CAMERA_QUALITIES.end()),
                step: 1.0,
                value: f64::from(settings.recording.camera_quality),
                onchange: {
                    let hub = hub.clone();
                    move |quality: f64| hub.update_settings(|s| s.recording.camera_quality = quality.round() as u8)
                },
            }
            NumberSlider {
                label: "Before points",
                title: "Footage the cameras keep from before each point starts",
                min: *RecordingSettings::MARGINS.start(),
                max: *RecordingSettings::MARGINS.end(),
                step: 0.5,
                value: settings.recording.margins.before.as_secs_f64(),
                decimals: 1,
                unit: " s",
                oninput: recording_time(|recording, before| {
                    recording.margins.before = before;
                    recording.buffer = recording.buffer.max(before);
                }),
            }
            NumberSlider {
                label: "After points",
                title: "Footage the cameras keep from after each point ends",
                min: *RecordingSettings::MARGINS.start(),
                max: *RecordingSettings::MARGINS.end(),
                step: 0.5,
                value: settings.recording.margins.after.as_secs_f64(),
                decimals: 1,
                unit: " s",
                oninput: recording_time(|recording, after| recording.margins.after = after),
            }
            NumberSlider {
                label: "Camera buffer",
                title: "How long the cameras hold frames before keeping or discarding them. Footage from before a \
                        point is only kept if a camera hears that the point started in time, so make this longer \
                        than \"Before points\" by a couple of seconds, more if Arctos is slow to reach. Longer \
                        takes more of the cameras' memory",
                min: settings.recording.margins.before.as_secs_f64().max(*RecordingSettings::BUFFERS.start()),
                max: *RecordingSettings::BUFFERS.end(),
                step: 0.5,
                value: settings.recording.buffer.as_secs_f64(),
                decimals: 1,
                unit: " s",
                oninput: recording_time(|recording, buffer| recording.buffer = buffer),
            }
            label { class: "row",
                title: "How finely stitched videos are compressed. Higher keeps more detail and makes bigger files",
                span { class: "label", "Video quality" }
                select {
                    onchange: {
                        let hub = hub.clone();
                        move |e: FormEvent| {
                            if let Ok(index) = e.value().parse::<usize>() {
                                hub.update_settings(|s| s.recording.video_quality = VideoQuality::ALL[index]);
                            }
                        }
                    },
                    for (index, (quality, name)) in VideoQuality::ALL.into_iter().zip(["Low", "Standard", "High", "Very high"]).enumerate() {
                        option { value: "{index}", selected: quality == settings.recording.video_quality, "{name}" }
                    }
                }
            }
        }
        fieldset {
            legend { "Scoreboard overlay" }
            label { class: "check",
                title: "Videos show the score as it was at each moment of the match; live streams show the field's \
                        live scoreboard",
                input {
                    r#type: "checkbox",
                    checked: settings.overlay.enabled,
                    onchange: {
                        let hub = hub.clone();
                        move |e: FormEvent| hub.update_settings(|s| s.overlay.enabled = e.checked())
                    },
                }
                "Draw the scoreboard onto videos and live streams"
            }
            if settings.overlay.enabled {
                AlignSelect {
                    label: "Horizontal",
                    title: "Whether the scoreboard sits against the left or right edge of the video, or is centered",
                    names: ["Left", "Center", "Right"],
                    value: settings.overlay.horizontal,
                    onchange: {
                        let hub = hub.clone();
                        move |a| hub.update_settings(|s| s.overlay.horizontal = a)
                    },
                }
                AlignSelect {
                    label: "Vertical",
                    title: "Whether the scoreboard sits against the top or bottom edge of the video, or is centered",
                    names: ["Top", "Center", "Bottom"],
                    value: settings.overlay.vertical,
                    onchange: {
                        let hub = hub.clone();
                        move |a| hub.update_settings(|s| s.overlay.vertical = a)
                    },
                }
                NumberSlider {
                    label: "X offset",
                    title: "How far in from that edge the scoreboard sits, in percent of the video's width (right of \
                            center, when centered)",
                    min: -50.0,
                    max: 50.0,
                    step: 0.5,
                    value: settings.overlay.x,
                    decimals: 1,
                    unit: "%",
                    oninput: overlay(|overlay, x| overlay.x = x),
                }
                NumberSlider {
                    label: "Y offset",
                    title: "How far in from that edge the scoreboard sits, in percent of the video's height (below \
                            center, when centered)",
                    min: -50.0,
                    max: 50.0,
                    step: 0.5,
                    value: settings.overlay.y,
                    decimals: 1,
                    unit: "%",
                    oninput: overlay(|overlay, y| overlay.y = y),
                }
                NumberSlider {
                    label: "Width",
                    title: "The scoreboard's width, in percent of the video's width",
                    min: 10.0,
                    max: 100.0,
                    step: 0.5,
                    value: settings.overlay.width,
                    decimals: 1,
                    unit: "%",
                    oninput: overlay(|overlay, width| overlay.width = width),
                }
            }
        }
    }
}

/// The wired port the cameras are on (see [`camera_port`]).
#[component]
fn CameraNetwork() -> Element {
    let hub = use_hub();
    let runtime = use_runtime();
    let port = use_watch(|hub| &hub.camera_port);
    let port = port.read();
    let mut choices = port.choices.clone();
    if let Some(chosen) = &port.chosen
        && !choices.contains(chosen)
    {
        choices.push(chosen.clone());
    }
    rsx! {
        fieldset {
            legend { "Camera network" }
            label { class: "row",
                title: "The wired port the cameras' switch is plugged into, which gets a self-assigned address to reach \
                        them by instead of waiting for a DHCP server. Choose a port only the cameras are on",
                span { class: "label", "Port" }
                select {
                    onchange: move |e: FormEvent| {
                        let port = Some(e.value()).filter(|port| !port.is_empty());
                        let hub = hub.clone();
                        runtime.spawn(async move { camera_port::choose(&hub, port).await });
                    },
                    option { value: "", selected: port.chosen.is_none(), "None" }
                    for choice in choices {
                        option {
                            selected: port.chosen.as_ref() == Some(&choice),
                            value: "{choice}",
                            "{choice}"
                        }
                    }
                }
            }
            if let Some(error) = &port.error {
                p { class: "error", "{error}" }
            }
        }
    }
}

#[component]
fn Account() -> Element {
    const TITLE: &str = "The Arctos account the base station reads the tournament with and uploads videos as. It \
                         must organize the tournament";
    let hub = use_hub();
    let status = use_watch(|hub| &hub.account.status);
    let sign_in = {
        let hub = hub.clone();
        move |_| hub.account.sign_in()
    };
    match status() {
        AccountStatus::SignedIn(user) => rsx! {
            div { class: "row", title: TITLE,
                span { class: "label", "Account" }
                span { class: "account", "{user.name}" }
                button {
                    onclick: move |_| {
                        let hub = hub.clone();
                        spawn(async move {
                            if let Err(e) = hub.account.sign_out().await {
                                tracing::warn!("signing out of Arctos: {e:#}");
                            }
                        });
                    },
                    "Sign out"
                }
            }
        },
        AccountStatus::SigningIn => rsx! {
            div { class: "row", title: TITLE,
                span { class: "label", "Account" }
                span { class: "account muted", "Allow access in your browser…" }
                button { onclick: move |_| hub.account.cancel_sign_in(), "Cancel" }
            }
        },
        AccountStatus::SignedOut { error } => rsx! {
            div { class: "row", title: TITLE,
                span { class: "label", "Account" }
                span { class: "account muted", "Not signed in" }
                button { onclick: sign_in, "Sign in with Arctos" }
            }
            if let Some(error) = error {
                p { class: "error", "{error}" }
            }
        },
    }
}

#[component]
fn WorkflowCheck(
    label: &'static str,
    title: String,
    checked: bool,
    onchange: EventHandler<bool>,
) -> Element {
    rsx! {
        label { class: "check", title,
            input {
                r#type: "checkbox",
                checked,
                onchange: move |e: FormEvent| onchange.call(e.checked()),
            }
            "{label}"
        }
    }
}

#[component]
fn TextInput(
    label: &'static str,
    title: &'static str,
    value: String,
    #[props(default = "text")] kind: &'static str,
    #[props(default)] placeholder: &'static str,
    onchange: EventHandler<String>,
) -> Element {
    rsx! {
        label { class: "row", title,
            span { class: "label", "{label}" }
            input {
                r#type: kind,
                placeholder,
                value,
                onchange: move |e| onchange.call(e.value()),
            }
        }
    }
}

#[component]
fn AlignSelect(
    label: &'static str,
    title: &'static str,
    names: [&'static str; 3],
    value: Align,
    onchange: EventHandler<Align>,
) -> Element {
    rsx! {
        label { class: "row", title,
            span { class: "label", "{label}" }
            select {
                onchange: move |e| {
                    if let Ok(index) = e.value().parse::<usize>() {
                        onchange.call(Align::ALL[index]);
                    }
                },
                for (index, (align, name)) in Align::ALL.into_iter().zip(names).enumerate() {
                    option { value: "{index}", selected: align == value, "{name}" }
                }
            }
        }
    }
}

/// Calls `onchange` only with numbers from `min` to `max`.
#[component]
fn NumberInput(
    label: &'static str,
    title: &'static str,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    #[props(default)] unit: &'static str,
    onchange: EventHandler<f64>,
) -> Element {
    rsx! {
        label { class: "row", title,
            span { class: "label", "{label}" }
            input {
                r#type: "number",
                min,
                max,
                step,
                value,
                onchange: move |e: FormEvent| {
                    if let Ok(number) = e.value().parse::<f64>()
                        && (min..=max).contains(&number)
                    {
                        onchange.call(number);
                    }
                },
            }
            span { "{unit}" }
        }
    }
}
