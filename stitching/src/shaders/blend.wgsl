// One level of the blend, collapsed as it goes: the cameras' bands of
// detail (each Gaussian level minus the next one upsampled), weighted, plus
// the coarser collapsed result upsampled.

@group(0) @binding(0) var left_fine: texture_2d<f32>;
@group(0) @binding(1) var left_coarse: texture_2d<f32>;
@group(0) @binding(2) var right_fine: texture_2d<f32>;
@group(0) @binding(3) var right_coarse: texture_2d<f32>;
@group(0) @binding(4) var weights: texture_2d<f32>;
@group(0) @binding(5) var result_coarse: texture_2d<f32>;
@group(0) @binding(6) var result: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(result);
    if any(id.xy >= size) {
        return;
    }
    let pixel = vec2<i32>(id.xy);
    let weight = textureLoad(weights, pixel, 0).xy;
    let total = weight.x + weight.y;
    var detail = vec3<f32>(0.0);
    if total > MIN_COVERAGE {
        let left = unpremultiply(textureLoad(left_fine, pixel, 0)) - upsample(left_coarse, pixel, size, true);
        let right = unpremultiply(textureLoad(right_fine, pixel, 0)) - upsample(right_coarse, pixel, size, true);
        detail = (weight.x * left + weight.y * right) / total;
    }
    textureStore(result, pixel, vec4<f32>(detail + upsample(result_coarse, pixel, size, false), 1.0));
}
