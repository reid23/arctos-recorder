// Samples one camera's frame at each output pixel through its lookup table,
// premultiplied by how much the camera covers the pixel: (Y, Cb, Cr, 1)
// times the coverage, with Cb and Cr centered on 0.

@group(0) @binding(0) var<storage, read> lut: array<vec4<f32>>;
@group(0) @binding(1) var luma: texture_2d<f32>;
@group(0) @binding(2) var chroma_blue: texture_2d<f32>;
@group(0) @binding(3) var chroma_red: texture_2d<f32>;
@group(0) @binding(4) var bilinear: sampler;
@group(0) @binding(5) var warped: texture_storage_2d<rgba16float, write>;

const CHROMA_ZERO: f32 = 128.0 / 255.0;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(warped);
    if any(id.xy >= size) {
        return;
    }
    let entry = lut[id.y * size.x + id.x];
    var color = vec4<f32>(0.0);
    if entry.z > 0.0 {
        let y = textureSampleLevel(luma, bilinear, entry.xy, 0.0).r;
        let blue = textureSampleLevel(chroma_blue, bilinear, entry.xy, 0.0).r - CHROMA_ZERO;
        let red = textureSampleLevel(chroma_red, bilinear, entry.xy, 0.0).r - CHROMA_ZERO;
        color = vec4<f32>(y, blue, red, 1.0) * entry.z;
    }
    textureStore(warped, id.xy, color);
}
