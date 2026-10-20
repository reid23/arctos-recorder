// Scales the feathered weights by each camera's coverage, so no camera is
// weighted where it has no image.

@group(0) @binding(0) var<storage, read> left_lut: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read> right_lut: array<vec4<f32>>;
@group(0) @binding(2) var feathered: texture_2d<f32>;
@group(0) @binding(3) var weights: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(weights);
    if any(id.xy >= size) {
        return;
    }
    let index = id.y * size.x + id.x;
    let coverage = vec2<f32>(left_lut[index].z, right_lut[index].z);
    let weight = textureLoad(feathered, vec2<i32>(id.xy), 0).xy * coverage;
    textureStore(weights, id.xy, vec4<f32>(weight, 0.0, 0.0));
}
