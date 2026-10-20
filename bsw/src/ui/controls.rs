//! Form controls shared by the main window and the calibration windows.

use dioxus::prelude::*;

/// A slider with its value beside it. Clicking the value lets another be
/// typed, more precisely than the slider's `step` if need be, which is
/// taken (with Enter, or by clicking away) only if it is from `min` to
/// `max`.
#[component]
pub fn NumberSlider(
    label: &'static str,
    title: Option<&'static str>,
    min: f64,
    max: f64,
    step: f64,
    value: f64,
    decimals: usize,
    unit: &'static str,
    oninput: EventHandler<f64>,
) -> Element {
    let mut editing = use_signal(|| false);
    // Only read outside rendering: rerendering the box resets what's typed in it.
    let mut typed = use_signal(String::new);
    let shown = format!("{value:.decimals$}");
    let accepted = move |text: &str| {
        text.trim()
            .parse::<f64>()
            .ok()
            .filter(|number| (min..=max).contains(number))
    };
    rsx! {
        label { class: "row", title,
            span { class: "label", "{label}" }
            input {
                r#type: "range",
                min,
                max,
                step,
                value,
                oninput: move |e| {
                    if let Ok(value) = e.value().parse::<f64>() {
                        oninput.call(value);
                    }
                },
            }
            if editing() {
                input {
                    class: "value",
                    r#type: "number",
                    min,
                    max,
                    step: "any",
                    value: "{shown}",
                    title: "From {min}{unit} to {max}{unit}",
                    onmounted: move |e| async move {
                        // Dioxus 0.7 focuses, then fails to read the webview's reply.
                        _ = e.set_focus(true).await;
                        _ = document::eval("document.activeElement.select()").await;
                    },
                    oninput: move |e| typed.set(e.value()),
                    onkeydown: move |e| match e.key() {
                        Key::Enter => {
                            if let Some(number) = accepted(&typed.read()) {
                                oninput.call(number);
                                editing.set(false);
                            }
                        }
                        Key::Escape => editing.set(false),
                        _ => {}
                    },
                    onblur: move |_| {
                        if editing() {
                            if let Some(number) = accepted(&typed.read()) {
                                oninput.call(number);
                            }
                            editing.set(false);
                        }
                    },
                }
            } else {
                span {
                    class: "value editable",
                    title: "Click to type a value",
                    onclick: {
                        let shown = shown.clone();
                        move |_| {
                            typed.set(shown.clone());
                            editing.set(true);
                        }
                    },
                    "{shown}{unit}"
                }
            }
        }
    }
}
