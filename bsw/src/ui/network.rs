//! The Network Usage pane: who is talking to whom, and how full the links are.

use dioxus::prelude::*;

use super::use_watch;
use crate::network::{CameraFlow, Internet, Limit, Usage};

const WIDTH: f64 = 780.0;
const CAMERA_X: f64 = 100.0;
const BASE_X: f64 = 310.0;
const CLOUD_X: f64 = 510.0;
const RIGHT_X: f64 = 680.0;
const LONE_BASE_X: f64 = 390.0;
const MARGIN: f64 = 40.0;
const ROW_PITCH: f64 = 58.0;

#[component]
pub fn NetworkUsage() -> Element {
    let usage = use_watch(|hub| &hub.network_usage);
    let usage = usage.read();
    if !usage.ready {
        return rsx! { p { class: "muted", "Measuring…" } };
    }
    let diagram = layout(&usage);
    rsx! {
        svg {
            class: "network-graph",
            view_box: "0 0 {WIDTH} {diagram.height}",
            style: "aspect-ratio: {WIDTH} / {diagram.height}",
            for edge in diagram.edges {
                line {
                    x1: "{edge.x1}",
                    y1: "{edge.y1}",
                    x2: "{edge.x2}",
                    y2: "{edge.y2}",
                    stroke: "#7d6c4c",
                    stroke_width: "1.5",
                }
                text {
                    x: "{(edge.x1 + edge.x2) / 2.0}",
                    y: "{(edge.y1 + edge.y2) / 2.0 - 8.0}",
                    text_anchor: "middle",
                    "{edge.forward}"
                }
                if !edge.back.is_empty() {
                    text {
                        x: "{(edge.x1 + edge.x2) / 2.0}",
                        y: "{(edge.y1 + edge.y2) / 2.0 + 14.0}",
                        text_anchor: "middle",
                        "{edge.back}"
                    }
                }
            }
            for node in diagram.nodes {
                rect {
                    x: "{node.x - node.width / 2.0}",
                    y: "{node.y - 14.0}",
                    width: "{node.width}",
                    height: "28",
                    rx: "4",
                    fill: "#f4f6d7",
                    stroke: "#584528",
                }
                text {
                    x: "{node.x}",
                    y: "{node.y + 4.0}",
                    text_anchor: "middle",
                    "{node.label}"
                }
            }
        }
        div { class: "constraints",
            if let Some(port) = usage.camera_port {
                p { "{port_text(&port)}" }
            }
            for camera in usage.cameras.iter().filter(|camera| camera.link.is_some()) {
                p { "{camera_link_text(camera)}" }
            }
            if let Some(internet) = &usage.internet {
                p { "{internet_text(internet)}" }
            }
            if let Some(upload) = usage.cloud_upload {
                p { "Cloud upload: {rate(upload)} sent to Arctos and stream servers." }
            }
        }
    }
}

struct Node {
    x: f64,
    y: f64,
    width: f64,
    label: String,
}

struct Edge {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    /// Toward the right-hand node.
    forward: String,
    /// Toward the left-hand node. Empty when nothing comes back.
    back: String,
}

struct Diagram {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    height: f64,
}

fn layout(usage: &Usage) -> Diagram {
    let rows = usage.cameras.len().max(1 + usage.streams.len()).max(2);
    let height = (rows as f64 * ROW_PITCH + MARGIN * 2.0).max(260.0);
    let spine = height / 2.0;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    let mut camera_indexes = Vec::new();
    for (camera, y) in usage.cameras.iter().zip(spread(usage.cameras.len(), height)) {
        camera_indexes.push(nodes.len());
        nodes.push(node(CAMERA_X, y, &camera.name));
    }
    let base_x = if usage.cloud.is_some() { BASE_X } else { LONE_BASE_X };
    let base = nodes.len();
    nodes.push(node(base_x, spine, "Base station"));
    for (camera, index) in usage.cameras.iter().zip(camera_indexes) {
        edges.push(connect(
            &nodes,
            index,
            base,
            rightward(camera.from_camera),
            leftward(camera.to_camera),
        ));
    }

    let cloud = usage.cloud.map(|flow| {
        let index = nodes.len();
        nodes.push(node(CLOUD_X, spine, "SSW"));
        edges.push(connect(&nodes, base, index, rightward(flow.sent), leftward(flow.received)));
        index
    });

    let mut right_ys = spread(1 + usage.streams.len(), height);
    // Arctos shares the spine's height with SSW only when SSW is absent.
    // Otherwise the base station's line to Arctos would run through SSW.
    if cloud.is_some() && right_ys.len() == 1 {
        right_ys[0] = MARGIN;
    }
    let arctos = nodes.len();
    nodes.push(node(RIGHT_X, right_ys[0], "Arctos"));
    let mut stream_indexes = Vec::new();
    for (stream, y) in usage.streams.iter().zip(right_ys.into_iter().skip(1)) {
        stream_indexes.push(nodes.len());
        nodes.push(node(RIGHT_X, y, &stream.name));
    }

    edges.push(connect(
        &nodes,
        base,
        arctos,
        rightward(usage.arctos.sent),
        leftward(usage.arctos.received),
    ));
    if let (Some(cloud_index), Some(flow)) = (cloud, usage.cloud_arctos) {
        edges.push(connect(
            &nodes,
            cloud_index,
            arctos,
            rightward(flow.sent),
            leftward(flow.received),
        ));
    }
    for (stream, index) in usage.streams.iter().zip(stream_indexes) {
        let source = if stream.from_cloud { cloud.unwrap_or(base) } else { base };
        edges.push(connect(&nodes, source, index, rightward(stream.megabits), String::new()));
    }
    Diagram { nodes, edges, height }
}

fn spread(count: usize, height: f64) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![height / 2.0];
    }
    let last = (count - 1) as f64;
    (0..count)
        .map(|index| MARGIN + (height - 2.0 * MARGIN) * index as f64 / last)
        .collect()
}

fn node(x: f64, y: f64, label: &str) -> Node {
    Node {
        x,
        y,
        width: (label.len() as f64 * 7.2 + 24.0).clamp(72.0, 160.0),
        label: label.to_owned(),
    }
}

fn connect(nodes: &[Node], from: usize, to: usize, forward: String, back: String) -> Edge {
    let start = border(&nodes[from], (nodes[to].x, nodes[to].y));
    let end = border(&nodes[to], (nodes[from].x, nodes[from].y));
    Edge {
        x1: start.0,
        y1: start.1,
        x2: end.0,
        y2: end.1,
        forward,
        back,
    }
}

/// Where a line from `node`'s center toward `toward` crosses the box.
fn border(node: &Node, toward: (f64, f64)) -> (f64, f64) {
    let dx = toward.0 - node.x;
    let dy = toward.1 - node.y;
    let reach_x = if dx == 0.0 {
        f64::INFINITY
    } else {
        node.width / 2.0 / dx.abs()
    };
    let reach_y = if dy == 0.0 { f64::INFINITY } else { 14.0 / dy.abs() };
    let scale = reach_x.min(reach_y);
    (node.x + dx * scale, node.y + dy * scale)
}

fn rightward(megabits: f64) -> String {
    format!("{} →", rate(megabits))
}

fn leftward(megabits: f64) -> String {
    format!("← {}", rate(megabits))
}

fn rate(megabits: f64) -> String {
    if megabits >= 100.0 {
        format!("{megabits:.0} Mb/s")
    } else if megabits >= 10.0 {
        format!("{megabits:.1} Mb/s")
    } else {
        format!("{megabits:.2} Mb/s")
    }
}

fn limit_suffix(limit: &Limit) -> String {
    limit
        .megabits
        .map(|megabits| format!(", of {:.0} Mb/s each way", megabits))
        .unwrap_or_default()
}

fn port_text(port: &Limit) -> String {
    format!(
        "Camera port: {} in, {} out{}",
        rate(port.received),
        rate(port.sent),
        limit_suffix(port)
    )
}

fn camera_link_text(camera: &CameraFlow) -> String {
    let Some(link) = camera.link else {
        return String::new();
    };
    format!(
        "{} link: {} sent, {} received{}",
        camera.name,
        rate(link.sent),
        rate(link.received),
        limit_suffix(&link)
    )
}

fn internet_text(internet: &Internet) -> String {
    format!(
        "Internet ({}): ours {} up and {} down; the adapter {} up and {} down.",
        internet.adapter,
        rate(internet.ours_sent),
        rate(internet.ours_received),
        rate(internet.adapter_sent),
        rate(internet.adapter_received)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{CameraFlow, Flow, StreamFlow, Usage};

    fn usage(cameras: &[&str], cloud: bool, streams: &[(&str, bool)]) -> Usage {
        Usage {
            ready: true,
            cameras: cameras
                .iter()
                .map(|name| CameraFlow {
                    name: (*name).to_owned(),
                    from_camera: 8.0,
                    to_camera: 0.2,
                    link: None,
                })
                .collect(),
            cloud: cloud.then_some(Flow { sent: 20.0, received: 1.0 }),
            arctos: Flow { sent: 0.4, received: 0.1 },
            cloud_arctos: cloud.then_some(Flow { sent: 2.0, received: 0.2 }),
            streams: streams
                .iter()
                .map(|(name, from_cloud)| StreamFlow {
                    name: (*name).to_owned(),
                    megabits: 6.0,
                    from_cloud: *from_cloud,
                })
                .collect(),
            camera_port: None,
            internet: None,
            cloud_upload: None,
        }
    }

    fn node_named<'a>(diagram: &'a Diagram, label: &str) -> &'a Node {
        diagram.nodes.iter().find(|node| node.label == label).unwrap()
    }

    /// The box is inset, so a line that only touches a node's border (its own
    /// endpoints) does not count.
    fn segment_enters(edge: &Edge, node: &Node) -> bool {
        let left = node.x - node.width / 2.0 + 1.0;
        let right = node.x + node.width / 2.0 - 1.0;
        let top = node.y - 14.0 + 1.0;
        let bottom = node.y + 14.0 - 1.0;
        if right <= left || bottom <= top {
            return false;
        }
        let mut enter = 0.0;
        let mut exit = 1.0;
        let mut clip = |direction: f64, distance: f64| -> bool {
            if direction == 0.0 {
                return distance >= 0.0;
            }
            let ratio = distance / direction;
            if direction < 0.0 {
                if ratio > exit {
                    return false;
                }
                if ratio > enter {
                    enter = ratio;
                }
            } else {
                if ratio < enter {
                    return false;
                }
                if ratio < exit {
                    exit = ratio;
                }
            }
            true
        };
        let dx = edge.x2 - edge.x1;
        let dy = edge.y2 - edge.y1;
        clip(-dx, edge.x1 - left)
            && clip(dx, right - edge.x1)
            && clip(-dy, edge.y1 - top)
            && clip(dy, bottom - edge.y1)
            && enter < exit
    }

    fn assert_lines_miss_other_nodes(diagram: &Diagram) {
        for edge in &diagram.edges {
            for node in &diagram.nodes {
                assert!(
                    !segment_enters(edge, node),
                    "{} / {} runs through {}",
                    edge.forward,
                    edge.back,
                    node.label
                );
            }
        }
    }

    #[test]
    fn lines_reach_their_nodes_without_crossing_the_others() {
        let through = Edge {
            x1: 0.0,
            y1: 0.0,
            x2: 20.0,
            y2: 0.0,
            forward: String::new(),
            back: String::new(),
        };
        let around = Node {
            x: 10.0,
            y: 0.0,
            width: 8.0,
            label: "box".into(),
        };
        assert!(segment_enters(&through, &around));
        let above = Edge {
            x1: 0.0,
            y1: 20.0,
            x2: 20.0,
            y2: 20.0,
            forward: String::new(),
            back: String::new(),
        };
        assert!(!segment_enters(&above, &around));

        let diagram = layout(&usage(&["ar-18a621", "ar-b2f94f"], true, &[("YouTube", true)]));
        assert!(node_named(&diagram, "ar-18a621").x < node_named(&diagram, "Base station").x);
        assert!(node_named(&diagram, "Base station").x < node_named(&diagram, "SSW").x);
        assert!(node_named(&diagram, "SSW").x < node_named(&diagram, "Arctos").x);
        assert_eq!(node_named(&diagram, "Arctos").x, node_named(&diagram, "YouTube").x);
        assert!(node_named(&diagram, "Arctos").y < node_named(&diagram, "SSW").y);
        assert!(node_named(&diagram, "YouTube").y > node_named(&diagram, "SSW").y);
        assert_lines_miss_other_nodes(&diagram);

        let local = layout(&usage(
            &["ar-18a621", "ar-b2f94f", "ar-sn1"],
            false,
            &[("YouTube", false)],
        ));
        assert!(node_named(&local, "ar-18a621").x < node_named(&local, "Base station").x);
        assert!(node_named(&local, "Base station").x < node_named(&local, "Arctos").x);
        assert!(local.nodes.iter().all(|node| node.label != "SSW"));
        assert_lines_miss_other_nodes(&local);

        assert_lines_miss_other_nodes(&layout(&usage(&["ar-18a621"], true, &[])));
        assert_lines_miss_other_nodes(&layout(&usage(
            &["a", "b", "c", "d"],
            true,
            &[("YouTube", true), ("Twitch", true)],
        )));
    }
}
