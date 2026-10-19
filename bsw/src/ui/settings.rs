//! Settings take effect as soon as they are changed.

use dioxus::prelude::*;

use super::{use_hub, use_watch};
use crate::account::AccountStatus;
use crate::settings::{Align, Settings as AllSettings, Stitching, Workflow};

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

    rsx! {
        fieldset {
            legend { "Arctos" }
            Account {}
            TextInput {
                label: "Tournament",
                value: settings.arctos.tournament.clone(),
                onchange: update(|s, v| s.arctos.tournament = v),
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
            legend { "Resource Usage" }
            WorkflowCheck {
                label: "Live stream",
                title: "Restream the camera views to their stream URLs",
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
                "Draw the scoreboard onto videos and live streams"
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
fn Account() -> Element {
    let hub = use_hub();
    let status = use_watch(|hub| &hub.account.status);
    let sign_in = {
        let hub = hub.clone();
        move |_| hub.account.sign_in()
    };
    match status() {
        AccountStatus::SignedIn(user) => rsx! {
            div { class: "row",
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
            div { class: "row",
                span { class: "label", "Account" }
                span { class: "account muted", "Allow access in your browser…" }
                button { onclick: move |_| hub.account.cancel_sign_in(), "Cancel" }
            }
        },
        AccountStatus::SignedOut { error } => rsx! {
            div { class: "row",
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
