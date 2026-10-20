// For each output pixel: where one camera saw it (texture coordinates), how
// much the camera covers it (fading out at the image's edge), and how far it
// is off the camera's optical axis. See `StitchConfig::source_of`.

struct Lut {
    // From the cylinder's frame to the camera's.
    rotation: mat3x3<f32>,
    focal: vec2<f32>,
    center: vec2<f32>,
    distortion: vec4<f32>,
    input_size: vec2<f32>,
    max_angle: f32,
    // Width in input pixels of the fade at the image's edge.
    edge: f32,
    output_size: vec2<u32>,
    // Output pixels per radian of the cylinder.
    output_focal: f32,
    // The cylinder's height at the middle row.
    middle_height: f32,
}

@group(0) @binding(0) var<uniform> params: Lut;
@group(0) @binding(1) var<storage, read_write> lut: array<vec4<f32>>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= params.output_size) {
        return;
    }
    let cylinder = (vec2<f32>(id.xy) + 0.5 - vec2<f32>(params.output_size) * 0.5) / params.output_focal;
    let height = params.middle_height + cylinder.y;
    let direction = params.rotation * vec3<f32>(sin(cylinder.x), height, cos(cylinder.x));
    let radius = length(direction.xy);
    let angle = atan2(radius, direction.z);
    var entry = vec4<f32>(0.0, 0.0, 0.0, angle);
    if angle < params.max_angle {
        let angle2 = angle * angle;
        let k = params.distortion;
        let distorted = angle * (1.0 + angle2 * (k.x + angle2 * (k.y + angle2 * (k.z + angle2 * k.w))));
        var toward = vec2<f32>(0.0);
        if radius > 0.0 {
            toward = direction.xy / radius;
        }
        let pixel = params.focal * distorted * toward + params.center;
        let inside = min(pixel + 0.5, params.input_size - 0.5 - pixel);
        let coverage = smoothstep(0.0, params.edge, min(inside.x, inside.y));
        entry = vec4<f32>((pixel + 0.5) / params.input_size, coverage, angle);
    }
    lut[id.y * params.output_size.x + id.x] = entry;
}
