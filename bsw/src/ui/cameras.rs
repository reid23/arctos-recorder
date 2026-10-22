use std::rc::Rc;

use dioxus::prelude::*;
use protocol::{DEFAULT_AUDIO_DEVICE, Directive};

use super::{Drag, Splitter, arctos_url, calibration, start_drag, use_hub, use_runtime, use_watch};
use crate::hub::FieldStatus;
use crate::server::Feed;
use crate::settings::{Align, Overlay, Stitching};
use stitching::overlay::PAGE_SIZE;

#[component]
pub fn Cameras() -> Element {
    let cameras = use_watch(|hub| &hub.cameras);
    let online: Vec<String> = cameras
        .read()
        .iter()
        .filter(|(_, status)| status.online)
        .map(|(id, _)| id.clone())
        .collect();
    rsx! {
        if online.is_empty() {
            p { class: "empty", "No cameras on the network. Cameras appear here as soon as they connect." }
        }
        div { class: "camera-grid",
            for id in online {
                Camera { key: "{id}", id }
            }
        }
    }
}

#[component]
fn Camera(id: String) -> Element {
    let hub = use_hub();
    let runtime = use_runtime();
    let cameras = use_watch(|hub| &hub.cameras);
    let settings = use_watch(|hub| &hub.settings);
    let fields = use_watch(|hub| &hub.fields);
    let arctos_fields = use_watch(|hub| &hub.arctos_fields);
    let mut view_width = use_signal(|| 0.0);
    let mut card = use_signal(|| None::<Rc<MountedData>>);
    let dragging = use_context::<Signal<Option<Drag>>>();

    let status = cameras.read().get(&id).cloned();
    let address = status.as_ref().map(|s| s.address);
    let hearing_audio = status.as_ref().is_some_and(|s| s.hearing_audio);
    let audio_devices = status.map(|s| s.audio_devices).unwrap_or_default();
    let settings = settings.read();
    let camera = settings.cameras.get(&id).cloned().unwrap_or_default();
    let field_status = camera
        .field
        .as_ref()
        .and_then(|f| fields.read().get(f).cloned());
    let available_fields = arctos_fields.read().clone().unwrap_or_default();
    // Stitching locally draws the overlay onto the preview itself.
    let scoreboard = camera
        .field
        .as_ref()
        .filter(|_| settings.overlay.enabled && settings.stitching == Stitching::Cloud)
        .map(|field| arctos_url(&arctos::scoreboard_path(&settings.arctos.tournament, field)));
    let overlay_style = overlay_style(&settings.overlay, view_width());

    let set_field = {
        let (hub, id) = (hub.clone(), id.clone());
        move |e: FormEvent| {
            let field = Some(e.value()).filter(|f| !f.is_empty());
            hub.update_settings(|s| s.cameras.entry(id.clone()).or_default().field = field);
        }
    };
    let set_stream_url = {
        let (hub, id) = (hub.clone(), id.clone());
        move |e: FormEvent| {
            let url = Some(e.value()).filter(|u| !u.is_empty());
            hub.update_settings(|s| s.cameras.entry(id.clone()).or_default().stream_url = url);
        }
    };
    let set_stream_key = {
        let (hub, id) = (hub.clone(), id.clone());
        move |e: FormEvent| {
            let key = Some(e.value()).filter(|k| !k.is_empty());
            hub.update_settings(|s| s.cameras.entry(id.clone()).or_default().stream_key = key);
        }
    };
    let swap_sides = {
        let (hub, id) = (hub.clone(), id.clone());
        move |_| {
            hub.update_settings(|s| {
                let setup = &mut s.cameras.entry(id.clone()).or_default().setup;
                setup.swap_sides = !setup.swap_sides;
            })
        }
    };
    let set_audio_device = {
        let (hub, id) = (hub.clone(), id.clone());
        move |e: FormEvent| {
            let device = Some(e.value()).filter(|d| d != DEFAULT_AUDIO_DEVICE);
            hub.update_settings(|s| s.cameras.entry(id.clone()).or_default().setup.audio_device = device);
        }
    };
    rsx! {
        div { class: "camera", onmounted: move |e| card.set(Some(e.data())),
            div { class: "camera-header",
                b { "{id}" }
                if let Some(address) = address {
                    span { class: "muted", "{address}" }
                }
                label { class: "field",
                    title: "The Arctos field this camera records: it records the field's matches and uploads each \
                            one to its match",
                    "Field "
                    select { onchange: set_field,
                        option { value: "", selected: camera.field.is_none(), "(none)" }
                        for name in available_fields {
                            option {
                                selected: camera.field.as_deref() == Some(name.as_str()),
                                value: "{name}",
                                "{name}"
                            }
                        }
                    }
                }
            }
            div {
                class: if settings.stitching == Stitching::Cloud { "viewer wide" } else { "viewer" },
                onresize: move |e| {
                    if let Ok(size) = e.get_content_box_size() {
                        view_width.set(size.width);
                    }
                },
                match settings.stitching {
                    Stitching::Local => rsx! {
                        img { class: "panorama", src: Feed::Panorama.url(&id) }
                    },
                    Stitching::Cloud => rsx! {
                        div { class: "pair",
                            img { src: Feed::Left.url(&id) }
                            img { src: Feed::Right.url(&id) }
                        }
                    },
                }
                if let Some(src) = scoreboard {
                    iframe { class: "scoreboard", style: overlay_style, src }
                }
            }
            div { class: "camera-footer",
                match field_status {
                    Some(status) => rsx! { FieldSummary { status } },
                    None => rsx! { p { class: "muted", "Not assigned to a field; not recording." } },
                }
                label { class: "stream",
                    title: if settings.workflow.live_stream {
                        "The RTMP server to restream this camera's stitched view to, e.g. YouTube Live's"
                    } else {
                        "Live streaming is off in the settings"
                    },
                    span { "Stream server" }
                    input {
                        r#type: "text",
                        placeholder: "rtmp://a.rtmp.youtube.com/live2",
                        value: camera.stream_url.clone().unwrap_or_default(),
                        disabled: !settings.workflow.live_stream,
                        onchange: set_stream_url,
                    }
                }
                label { class: "stream",
                    title: if settings.workflow.live_stream {
                        "The key the stream server gave for this camera's stream"
                    } else {
                        "Live streaming is off in the settings"
                    },
                    span { "Stream key" }
                    input {
                        r#type: "password",
                        autocomplete: "off",
                        value: camera.stream_key.clone().unwrap_or_default(),
                        disabled: !settings.workflow.live_stream,
                        onchange: set_stream_key,
                    }
                }
                div { class: "camera-controls",
                    button {
                        title: "Show the left camera's feed on the right and vice versa",
                        onclick: swap_sides,
                        "Swap sides"
                    }
                    label {
                        title: "Which of the camera's microphones records the matches' sound",
                        "Microphone "
                        select { onchange: set_audio_device,
                            option {
                                value: DEFAULT_AUDIO_DEVICE,
                                selected: camera.setup.audio_device.is_none(),
                                "ALSA default"
                            }
                            for device in audio_devices {
                                option {
                                    selected: camera.setup.audio_device.as_ref() == Some(&device.name),
                                    value: "{device.name}",
                                    "{device.description}"
                                }
                            }
                        }
                    }
                    if !hearing_audio {
                        span { class: "error", "No audio" }
                    }
                    button {
                        title: "Calibrate the camera's lenses and alignment, and choose its view and blend",
                        onclick: {
                            let (hub, id) = (hub.clone(), id.clone());
                            move |_| calibration::open(hub.clone(), runtime.clone(), id.clone())
                        },
                        "Calibration…"
                    }
                }
            }
            div {
                class: "resize-corner",
                title: "Drag to resize the cameras",
                onmousedown: move |_| start_drag(dragging, Splitter::CameraCorner, card()),
            }
        }
    }
}

/// Places the scoreboard page over a `view_width` wide view the way it is
/// drawn onto the video.
fn overlay_style(overlay: &Overlay, view_width: f64) -> String {
    let (width, height) = PAGE_SIZE;
    let scale = view_width * overlay.width / 100.0 / width as f64;
    let place = |align: Align, offset: f64, [start, end]: [&'static str; 2]| match align {
        Align::Start => (format!("{start}: {offset}%"), start, "0"),
        Align::Center => (format!("{start}: calc(50% + {offset}%)"), "center", "-50%"),
        Align::End => (format!("{end}: {offset}%"), end, "0"),
    };
    let (x, x_origin, x_shift) = place(overlay.horizontal, overlay.x, ["left", "right"]);
    let (y, y_origin, y_shift) = place(overlay.vertical, overlay.y, ["top", "bottom"]);
    format!(
        "{x}; {y}; width: {width}px; height: {height}px; \
         transform-origin: {x_origin} {y_origin}; \
         transform: translate({x_shift}, {y_shift}) scale({scale});"
    )
}

#[component]
fn FieldSummary(status: FieldStatus) -> Element {
    let text = match &status.directive {
        Directive::Idle => "No match in progress".to_owned(),
        Directive::Record { match_id, play } => {
            let title = status.match_title.as_deref().unwrap_or(match_id);
            let ongoing = play.last().is_some_and(|span| span.end.is_none());
            let state = if ongoing {
                "point in progress"
            } else {
                "between points"
            };
            format!("Recording {title}: {} points, {state}", play.len())
        }
    };
    rsx! {
        p { class: if matches!(status.directive, Directive::Record { .. }) { "recording" }, "{text}" }
        if let Some(error) = status.error {
            p { class: "error", "Can't reach Arctos; following the last known state. {error}" }
        }
    }
}
