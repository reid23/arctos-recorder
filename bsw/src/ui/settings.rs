//! Settings take effect as soon as they are changed.

use dioxus::prelude::*;

use super::{use_hub, use_watch};
use crate::settings::{Align, Settings as AllSettings, Stitching};

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

    rsx! {
        fieldset {
            legend { "Arctos" }
            TextInput {
                label: "Tournament",
                value: settings.arctos.tournament.clone(),
                onchange: update(|s, v| s.arctos.tournament = v),
            }
            TextInput {
                label: "Organizer",
                value: settings.arctos.credentials.username.clone(),
                onchange: update(|s, v| s.arctos.credentials.username = v),
            }
            TextInput {
                label: "Password",
                kind: "password",
                value: settings.arctos.credentials.password.clone(),
                onchange: update(|s, v| s.arctos.credentials.password = v),
            }
            match &*fields.read() {
                Ok(fields) if fields.is_empty() => rsx! { p { class: "muted", "No fields found." } },
                Ok(fields) => rsx! { p { class: "muted", "Fields: {fields.join(\", \")}" } },
                Err(error) => rsx! { p { class: "error", "Can't load the tournament's fields: {error}" } },
            }
        }
        fieldset {
            legend { "Stitching" }
            div { class: "row",
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
            if settings.stitching == Stitching::Cloud {
                TextInput {
                    label: "Server URL",
                    placeholder: "http://stitcher.example.org:7800",
                    value: settings.cloud.url.clone(),
                    onchange: update(|s, v| s.cloud.url = v),
                }
                TextInput {
                    label: "Token",
                    kind: "password",
                    value: settings.cloud.token.clone(),
                    onchange: update(|s, v| s.cloud.token = v),
                }
            }
        }
        fieldset {
            legend { "Scoreboard overlay" }
            label { class: "check",
                input {
                    r#type: "checkbox",
                    checked: settings.overlay.enabled,
                    onchange: {
                        let hub = hub.clone();
                        move |e: FormEvent| hub.update_settings(|s| s.overlay.enabled = e.checked())
                    },
                }
                "Show the scoreboard on the camera views"
            }
            if settings.overlay.enabled {
                AlignSelect {
                    label: "Horizontal",
                    names: ["Left", "Center", "Right"],
                    value: settings.overlay.horizontal,
                    onchange: {
                        let hub = hub.clone();
                        move |a| hub.update_settings(|s| s.overlay.horizontal = a)
                    },
                }
                AlignSelect {
                    label: "Vertical",
                    names: ["Top", "Center", "Bottom"],
                    value: settings.overlay.vertical,
                    onchange: {
                        let hub = hub.clone();
                        move |a| hub.update_settings(|s| s.overlay.vertical = a)
                    },
                }
                Slider {
                    label: "X offset",
                    min: -50.0,
                    max: 50.0,
                    value: settings.overlay.x,
                    oninput: update(|s, v| s.overlay.x = v.parse().unwrap_or(s.overlay.x)),
                }
                Slider {
                    label: "Y offset",
                    min: -50.0,
                    max: 50.0,
                    value: settings.overlay.y,
                    oninput: update(|s, v| s.overlay.y = v.parse().unwrap_or(s.overlay.y)),
                }
                Slider {
                    label: "Width",
                    min: 10.0,
                    max: 100.0,
                    value: settings.overlay.width,
                    oninput: update(|s, v| s.overlay.width = v.parse().unwrap_or(s.overlay.width)),
                }
            }
        }
    }
}

#[component]
fn TextInput(
    label: &'static str,
    value: String,
    #[props(default = "text")] kind: &'static str,
    #[props(default)] placeholder: &'static str,
    onchange: EventHandler<String>,
) -> Element {
    rsx! {
        label { class: "row",
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
    names: [&'static str; 3],
    value: Align,
    onchange: EventHandler<Align>,
) -> Element {
    rsx! {
        label { class: "row",
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

#[component]
fn Slider(
    label: &'static str,
    min: f64,
    max: f64,
    value: f64,
    oninput: EventHandler<String>,
) -> Element {
    rsx! {
        label { class: "row",
            span { class: "label", "{label}" }
            input {
                r#type: "range",
                min,
                max,
                step: 0.5,
                value,
                oninput: move |e| oninput.call(e.value()),
            }
            span { class: "value", "{value}%" }
        }
    }
}
