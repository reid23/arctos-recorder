use dioxus::prelude::*;
use protocol::Directive;

use super::{arctos_url, use_hub, use_watch};
use crate::hub::FieldStatus;
use crate::server::Feed;
use crate::settings::{Align, Overlay, Stitching};

/// Size the scoreboard page is laid out at before being scaled to fit.
/// Wide enough for the between-matches layout with long team names.
const SCOREBOARD_SIZE: (f64, f64) = (860.0, 200.0);

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
    let cameras = use_watch(|hub| &hub.cameras);
    let settings = use_watch(|hub| &hub.settings);
    let fields = use_watch(|hub| &hub.fields);
    let arctos_fields = use_watch(|hub| &hub.arctos_fields);
    let mut view_width = use_signal(|| 0.0);

    let address = cameras.read().get(&id).map(|s| s.address);
    let settings = settings.read();
    let camera = settings.cameras.get(&id).cloned().unwrap_or_default();
    let field_status = camera
        .field
        .as_ref()
        .and_then(|f| fields.read().get(f).cloned());
    let available_fields = arctos_fields.read().clone().unwrap_or_default();
    let scoreboard = camera
        .field
        .as_ref()
        .filter(|_| settings.overlay.enabled)
        .map(|field| {
            arctos_url(
                &arctos::scoreboard_path(&settings.arctos.tournament, field),
                settings.overlay.vertical,
            )
        });
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

    rsx! {
        div { class: "camera",
            div { class: "camera-header",
                b { "{id}" }
                if let Some(address) = address {
                    span { class: "muted", "{address}" }
                }
                label { class: "field",
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
                    "Restream to "
                    input {
                        r#type: "text",
                        placeholder: "rtmp://a.rtmp.youtube.com/live2/…",
                        value: camera.stream_url.clone().unwrap_or_default(),
                        onchange: set_stream_url,
                    }
                }
            }
        }
    }
}

/// Places the scoreboard page, laid out at [`SCOREBOARD_SIZE`], over a
/// `view_width` wide view.
fn overlay_style(overlay: &Overlay, view_width: f64) -> String {
    let scale = view_width * overlay.width / 100.0 / SCOREBOARD_SIZE.0;
    let place = |align: Align, offset: f64, [start, end]: [&'static str; 2]| match align {
        Align::Start => (format!("{start}: {offset}%"), start, "0"),
        Align::Center => (format!("{start}: calc(50% + {offset}%)"), "center", "-50%"),
        Align::End => (format!("{end}: {offset}%"), end, "0"),
    };
    let (x, x_origin, x_shift) = place(overlay.horizontal, overlay.x, ["left", "right"]);
    let (y, y_origin, y_shift) = place(overlay.vertical, overlay.y, ["top", "bottom"]);
    let (width, height) = SCOREBOARD_SIZE;
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
