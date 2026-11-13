use std::rc::Rc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use dioxus::prelude::*;
use protocol::rig::AUDIO_SAMPLE_RATE;
use protocol::{DEFAULT_AUDIO_DEVICE, Directive};

use super::{Drag, Splitter, arctos_url, calibration, start_drag, use_hub, use_runtime, use_watch};
use crate::hub::FieldStatus;
use crate::server::Feed;
use crate::settings::{Align, Overlay, Stitching};
use stitching::overlay::PAGE_SIZE;

#[component]
pub fn Cameras() -> Element {
    let cameras = use_watch(|hub| &hub.cameras);
    let settings = use_watch(|hub| &hub.settings);
    let fields = use_watch(|hub| &hub.fields);
    let shown: Vec<String> = {
        let cameras = cameras.read();
        let settings = settings.read();
        let fields = fields.read();
        cameras
            .iter()
            .filter(|(id, status)| {
                if status.online {
                    return true;
                }
                // A camera that has dropped off the network may still be
                // recording, and the operator has to be able to finish it.
                let Some(field) = settings.cameras.get(id.as_str()).and_then(|camera| camera.field.as_deref()) else {
                    return false;
                };
                matches!(
                    fields.get(field).map(|status| &status.directive),
                    Some(Directive::Record { .. } | Directive::Finalize { .. })
                )
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    rsx! {
        if shown.is_empty() {
            p { class: "empty", "No cameras on the network. Cameras appear here as soon as they connect." }
        }
        div { class: "camera-grid",
            for id in shown {
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
    let online = status.as_ref().is_some_and(|status| status.online);
    let address = status.as_ref().map(|s| s.address);
    let hearing_audio = status.as_ref().is_some_and(|s| s.hearing_audio);
    let software_id = status.as_ref().and_then(|status| status.software_id.clone());
    let audio_devices = status.map(|s| s.audio_devices).unwrap_or_default();
    let expected_software_id = hub.expected_camera_software_id.as_ref();
    let software_outdated = expected_software_id
        .is_some_and(|expected| software_id.as_ref() != Some(expected));
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
    let mut mic_test = use_signal(|| MicTest::Idle);
    let test_microphone = {
        let (hub, id, runtime) = (hub.clone(), id.clone(), runtime.clone());
        move |_| {
            if mic_test() != MicTest::Idle {
                return;
            }
            mic_test.set(MicTest::Recording);
            // The webview only plays sound from a context resumed during this click.
            let prepare = document::eval(
                "if (!window.micTestAudio) window.micTestAudio = new AudioContext(); await window.micTestAudio.resume();",
            );
            let (hub, id) = (hub.clone(), id.clone());
            let (sender, receiver) = tokio::sync::oneshot::channel();
            {
                let id = id.clone();
                runtime.spawn(async move {
                    let _ = sender.send(record_microphone(&hub, &id).await);
                });
            }
            spawn(async move {
                let recorded = receiver.await.ok().flatten();
                if let Err(error) = prepare.await {
                    tracing::warn!("preparing microphone playback: {error}");
                }
                let Some(wav) = recorded else {
                    tracing::warn!("no live audio from {id} to test");
                    mic_test.set(MicTest::Idle);
                    return;
                };
                mic_test.set(MicTest::Playing);
                let play = document::eval(PLAY_MICROPHONE);
                if let Err(error) = play.send(BASE64.encode(wav)) {
                    tracing::warn!("sending the microphone test: {error}");
                    mic_test.set(MicTest::Idle);
                    return;
                }
                if let Err(error) = play.await {
                    tracing::warn!("playing the microphone test: {error}");
                }
                mic_test.set(MicTest::Idle);
            });
        }
    };
    let mic_label = match mic_test() {
        MicTest::Idle => "Test mic",
        MicTest::Recording => "Recording…",
        MicTest::Playing => "Playing…",
    };
    rsx! {
        div { class: "camera", onmounted: move |e| card.set(Some(e.data())),
            div { class: "camera-header",
                b { "{id}" }
                if let Some(address) = address {
                    span { class: "muted", "{address}" }
                }
                if !online {
                    span { class: "muted", "offline" }
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
                    div { class: "camera-actions",
                        if software_outdated {
                            span { class: "error", "Camera software is out of date. Update it." }
                        }
                        button {
                            title: "Show the left camera's feed on the right and vice versa",
                            onclick: swap_sides,
                            "Swap sides"
                        }
                        button {
                            title: "Calibrate the camera's lenses and alignment, and choose its view and blend",
                            onclick: {
                                let (hub, id) = (hub.clone(), id.clone());
                                move |_| calibration::open(hub.clone(), runtime.clone(), id.clone())
                            },
                            "Calibrate cameras"
                        }
                    }
                    div { class: "mic",
                        label {
                            class: if hearing_audio { "" } else { "error" },
                            title: if hearing_audio {
                                "Which of the camera's microphones records the matches' sound"
                            } else {
                                "No audio is arriving from this camera"
                            },
                            "mic:"
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
                        button {
                            title: "Record three seconds of the audio arriving now and play them back",
                            disabled: mic_test() != MicTest::Idle,
                            onclick: test_microphone,
                            "{mic_label}"
                        }
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

#[derive(Clone, Copy, PartialEq)]
enum MicTest {
    Idle,
    Recording,
    Playing,
}

const MIC_TEST: Duration = Duration::from_secs(3);

const PLAY_MICROPHONE: &str = r#"
const binary = atob(await dioxus.recv());
const bytes = new Uint8Array(binary.length);
for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
const audio = window.micTestAudio;
await audio.resume();
const buffer = await audio.decodeAudioData(bytes.buffer);
const source = audio.createBufferSource();
source.buffer = buffer;
source.connect(audio.destination);
await new Promise((resolve) => {
    source.onended = resolve;
    source.start();
});
"#;

async fn record_microphone(hub: &crate::hub::Hub, camera: &str) -> Option<Vec<u8>> {
    let link = hub.link(camera)?;
    let mut incoming = link.audio.subscribe();
    let mut pcm = Vec::new();
    let deadline = tokio::time::Instant::now() + MIC_TEST;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, incoming.recv()).await {
            Ok(Ok(chunk)) => pcm.extend_from_slice(&chunk),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            _ => break,
        }
    }
    Some(microphone_wav(&pcm))
}

fn microphone_wav(pcm: &[u8]) -> Vec<u8> {
    let samples = AUDIO_SAMPLE_RATE as usize * 2 * MIC_TEST.as_secs() as usize;
    let mut body = Vec::with_capacity(samples);
    body.extend_from_slice(&pcm[..pcm.len().min(samples)]);
    body.resize(samples, 0);

    let mut wav = Vec::with_capacity(44 + body.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + body.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&AUDIO_SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(AUDIO_SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wav.extend_from_slice(&body);
    wav
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
    let hub = use_hub();
    let text = match &status.directive {
        Directive::Idle => "No match in progress".to_owned(),
        Directive::Record { match_id, play } | Directive::Finalize { match_id, play } => {
            let title = status.match_title.as_deref().unwrap_or(match_id);
            let ongoing = play.last().is_some_and(|span| span.end.is_none());
            let state = if ongoing { "point in progress" } else { "between points" };
            let verb = if matches!(status.directive, Directive::Finalize { .. }) {
                "Finishing"
            } else {
                "Recording"
            };
            format!("{verb} {title}: {} points, {state}", play.len())
        }
    };
    let finish = if let Directive::Record { match_id, .. } = status.directive.clone() {
        Some(match_id)
    } else {
        None
    };
    rsx! {
        p { class: if matches!(status.directive, Directive::Record { .. }) { "recording" }, "{text}" }
        if let Some(match_id) = finish {
            button {
                title: "End this recording and upload what it has. The match on Arctos is left as it is",
                onclick: {
                    let hub = hub.clone();
                    move |_| hub.finish_recording(&match_id)
                },
                "Finish recording"
            }
        }
        if let Some(error) = status.error {
            p { class: "error", "Can't reach Arctos; following the last known state. {error}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_microphone_test_is_three_seconds_of_audio() {
        let samples = AUDIO_SAMPLE_RATE as usize * 2 * 3;
        let wav = microphone_wav(&[1, 2, 3, 4]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + samples);
        assert_eq!(&wav[44..48], &[1, 2, 3, 4]);
        assert!(wav[48..].iter().all(|byte| *byte == 0));

        let wav = microphone_wav(&vec![7u8; samples + 10]);
        assert_eq!(wav.len(), 44 + samples);
        assert!(wav[44..].iter().all(|byte| *byte == 7));
    }
}
