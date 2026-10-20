// One direction of a separable Gaussian blur, sampled every `step` source
// pixels along it (1, or 2 to halve the image), clamping at the edges.

struct Blur {
    direction: vec2<i32>,
    radius: i32,
    sigma: f32,
    step: i32,
}

@group(0) @binding(0) var<uniform> params: Blur;
@group(0) @binding(1) var source: texture_2d<f32>;
@group(0) @binding(2) var destination: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(destination)) {
        return;
    }
    let last = vec2<i32>(textureDimensions(source)) - 1;
    let center = vec2<i32>(id.xy) * (vec2<i32>(1) + (params.step - 1) * params.direction);
    var sum = vec4<f32>(0.0);
    var total = 0.0;
    for (var offset = -params.radius; offset <= params.radius; offset++) {
        let distance = f32(offset);
        let weight = exp(-distance * distance / (2.0 * params.sigma * params.sigma));
        let pixel = clamp(center + offset * params.direction, vec2<i32>(0), last);
        sum += weight * textureLoad(source, pixel, 0);
        total += weight;
    }
    textureStore(destination, id.xy, sum / total);
}
