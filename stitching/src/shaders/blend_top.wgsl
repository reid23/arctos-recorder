// The coarsest level of the blend: the cameras' coarsest Gaussian levels,
// weighted.

@group(0) @binding(0) var left: texture_2d<f32>;
@group(0) @binding(1) var right: texture_2d<f32>;
@group(0) @binding(2) var weights: texture_2d<f32>;
@group(0) @binding(3) var result: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(result)) {
        return;
    }
    let pixel = vec2<i32>(id.xy);
    let weight = textureLoad(weights, pixel, 0).xy;
    let total = weight.x + weight.y;
    var color = vec3<f32>(0.0);
    if total > MIN_COVERAGE {
        color = (weight.x * unpremultiply(textureLoad(left, pixel, 0))
            + weight.y * unpremultiply(textureLoad(right, pixel, 0))) / total;
    }
    textureStore(result, pixel, vec4<f32>(color, 1.0));
}
