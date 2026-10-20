// Finishes the stitched frame (black where neither camera sees, the overlay
// drawn on top) and packs it into a planar YUV 4:2:0 frame: the Y plane,
// then Cb, then Cr, each byte a sample. For video, colors are converted from
// JPEG's YCbCr to HD video's (BT.709, limited range).

struct Pack {
    size: vec2<u32>,
    video: u32,
    overlay_shown: u32,
    overlay_origin: vec2<i32>,
    padding: vec2<u32>,
}

@group(0) @binding(0) var<uniform> params: Pack;
@group(0) @binding(1) var result: texture_2d<f32>;
@group(0) @binding(2) var<storage, read> left_lut: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> right_lut: array<vec4<f32>>;
@group(0) @binding(4) var overlay: texture_2d<f32>;
@group(0) @binding(5) var<storage, read_write> output: array<u32>;

// JPEG's (BT.601, full range) YCbCr of an RGB color, with Cb and Cr centered
// on 0.
fn rgb_to_jpeg(rgb: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(rgb, vec3<f32>(0.299, 0.587, 0.114)),
        dot(rgb, vec3<f32>(-0.168736, -0.331264, 0.5)),
        dot(rgb, vec3<f32>(0.5, -0.418688, -0.081312)),
    );
}

fn jpeg_to_rgb(color: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        color.x + 1.402 * color.z,
        color.x - 0.344136 * color.y - 0.714136 * color.z,
        color.x + 1.772 * color.y,
    );
}

// HD video's (BT.709) YCbCr of an RGB color, with Cb and Cr centered on 0.
fn rgb_to_video(rgb: vec3<f32>) -> vec3<f32> {
    let y = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec3<f32>(y, (rgb.b - y) / 1.8556, (rgb.r - y) / 1.5748);
}

fn color_at(pixel: vec2<i32>) -> vec3<f32> {
    let index = u32(pixel.y) * params.size.x + u32(pixel.x);
    let coverage = max(left_lut[index].z, right_lut[index].z);
    var color = textureLoad(result, pixel, 0).xyz * coverage;
    if params.overlay_shown != 0u {
        let local = pixel - params.overlay_origin;
        if all(local >= vec2<i32>(0)) && all(local < vec2<i32>(textureDimensions(overlay))) {
            let drawn = textureLoad(overlay, local, 0);
            color = mix(color, rgb_to_jpeg(drawn.rgb), drawn.a);
        }
    }
    if params.video != 0u {
        color = rgb_to_video(jpeg_to_rgb(color));
    }
    return color;
}

fn luma_byte(value: f32) -> u32 {
    let scaled = select(value * 255.0, 16.0 + value * 219.0, params.video != 0u);
    return u32(clamp(round(scaled), 0.0, 255.0));
}

fn chroma_byte(value: f32) -> u32 {
    let scaled = 128.0 + value * select(255.0, 224.0, params.video != 0u);
    return u32(clamp(round(scaled), 0.0, 255.0));
}

// Each invocation packs four luma samples side by side into a word.
@compute @workgroup_size(8, 8)
fn luma(@builtin(global_invocation_id) id: vec3<u32>) {
    let words = params.size.x / 4u;
    if id.x >= words || id.y >= params.size.y {
        return;
    }
    var word = 0u;
    for (var index = 0u; index < 4u; index++) {
        let pixel = vec2<i32>(i32(id.x * 4u + index), i32(id.y));
        word |= luma_byte(color_at(pixel).x) << (8u * index);
    }
    output[id.y * words + id.x] = word;
}

// Each invocation packs four chroma samples side by side into a word per
// plane, each sample averaging two by two pixels.
@compute @workgroup_size(8, 8)
fn chroma(@builtin(global_invocation_id) id: vec3<u32>) {
    let words = params.size.x / 8u;
    if id.x >= words || id.y >= params.size.y / 2u {
        return;
    }
    var blue = 0u;
    var red = 0u;
    for (var index = 0u; index < 4u; index++) {
        let corner = vec2<i32>(i32((id.x * 4u + index) * 2u), i32(id.y * 2u));
        let sum = color_at(corner).yz + color_at(corner + vec2<i32>(1, 0)).yz
            + color_at(corner + vec2<i32>(0, 1)).yz + color_at(corner + vec2<i32>(1, 1)).yz;
        blue |= chroma_byte(sum.x / 4.0) << (8u * index);
        red |= chroma_byte(sum.y / 4.0) << (8u * index);
    }
    let luma_words = params.size.x * params.size.y / 4u;
    let plane_words = luma_words / 4u;
    output[luma_words + id.y * words + id.x] = blue;
    output[luma_words + plane_words + id.y * words + id.x] = red;
}
