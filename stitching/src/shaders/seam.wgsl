// Chooses a camera for each output pixel: whichever covers it, or where both
// do, whichever sees it closer to its optical axis. Weights go in x (left)
// and y (right).

@group(0) @binding(0) var<storage, read> left_lut: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> right_lut: array<vec4<f32>>;
@group(0) @binding(2) var weights: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(weights);
    if any(id.xy >= size) {
        return;
    }
    let index = id.y * size.x + id.x;
    let left = left_lut[index];
    let right = right_lut[index];
    var weight = vec2<f32>(0.0);
    if left.z > 0.0 && (right.z <= 0.0 || left.w <= right.w) {
        weight.x = 1.0;
    } else if right.z > 0.0 {
        weight.y = 1.0;
    }
    textureStore(weights, id.xy, vec4<f32>(weight, 0.0, 0.0));
}
