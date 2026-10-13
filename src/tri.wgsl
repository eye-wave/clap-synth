@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var p = array<vec2<f32>, 3>(vec2(0.0, -0.6), vec2(0.6, 0.6), vec2(-0.6, 0.6));
    return vec4(p[i], 0.0, 1.0);
}

@fragment
fn fs() -> @location(0) vec4<f32> {
    return vec4(1.0, 0.5, 0.1, 1.0);
}
