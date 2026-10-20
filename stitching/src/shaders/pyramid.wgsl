// Moving between the blend's levels. Each level is either the same size as
// the next finer one or half its size (rounding up), its texel c lying at
// the finer level's texel 2c.

// Below this coverage a premultiplied color is treated as empty.
const MIN_COVERAGE: f32 = 0.001;

fn unpremultiply(color: vec4<f32>) -> vec3<f32> {
    if color.w > MIN_COVERAGE {
        return color.xyz / color.w;
    }
    return vec3<f32>(0.0);
}

// Weight of the coarse texel `offset` (-1, 0 or 1) from floor(x / 2) in
// Burt and Adelson's EXPAND at a fine pixel x.
fn expand_weight(odd: bool, offset: i32) -> f32 {
    if odd {
        return select(0.0, 0.5, offset == 0 || offset == 1);
    }
    return select(0.125, 0.75, offset == 0);
}

// The next coarser level `coarse` at `pixel` of a level `size` texels big,
// unpremultiplying each texel first if `premultiplied`: expanded if it's
// half the size, as it is otherwise.
fn upsample(coarse: texture_2d<f32>, pixel: vec2<i32>, size: vec2<u32>, premultiplied: bool) -> vec3<f32> {
    if all(textureDimensions(coarse) == size) {
        let texel = textureLoad(coarse, pixel, 0);
        if premultiplied {
            return unpremultiply(texel);
        }
        return texel.xyz;
    }
    let last = vec2<i32>(textureDimensions(coarse)) - 1;
    let half = pixel / 2;
    let odd = (pixel % 2) == vec2<i32>(1);
    var sum = vec3<f32>(0.0);
    for (var dy = -1; dy <= 1; dy++) {
        let weight_y = expand_weight(odd.y, dy);
        if weight_y == 0.0 {
            continue;
        }
        for (var dx = -1; dx <= 1; dx++) {
            let weight_x = expand_weight(odd.x, dx);
            if weight_x == 0.0 {
                continue;
            }
            let texel = textureLoad(coarse, clamp(half + vec2<i32>(dx, dy), vec2<i32>(0), last), 0);
            var color = texel.xyz;
            if premultiplied {
                color = unpremultiply(texel);
            }
            sum += weight_x * weight_y * color;
        }
    }
    return sum;
}
