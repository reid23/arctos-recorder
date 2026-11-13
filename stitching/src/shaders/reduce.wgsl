// One direction of Burt and Adelson's REDUCE: the 5-tap binomial kernel
// (1, 4, 6, 4, 1) / 16, sampled at every other source pixel along it,
// clamping at the edges. Its variance is one source pixel squared.

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var destination: texture_storage_2d<rgba16float, write>;

fn reduce(id: vec2<u32>, direction: vec2<i32>) {
    if any(id >= textureDimensions(destination)) {
        return;
    }
    let last = vec2<i32>(textureDimensions(source)) - 1;
    let center = vec2<i32>(id) * (vec2<i32>(1) + direction);
    let near = textureLoad(source, clamp(center - direction, vec2<i32>(0), last), 0)
        + textureLoad(source, clamp(center + direction, vec2<i32>(0), last), 0);
    let far = textureLoad(source, clamp(center - 2 * direction, vec2<i32>(0), last), 0)
        + textureLoad(source, clamp(center + 2 * direction, vec2<i32>(0), last), 0);
    let middle = textureLoad(source, center, 0);
    textureStore(destination, id, 0.375 * middle + 0.25 * near + 0.0625 * far);
}

@compute @workgroup_size(8, 8)
fn across(@builtin(global_invocation_id) id: vec3<u32>) {
    reduce(id.xy, vec2<i32>(1, 0));
}

@compute @workgroup_size(8, 8)
fn down(@builtin(global_invocation_id) id: vec3<u32>) {
    reduce(id.xy, vec2<i32>(0, 1));
}
