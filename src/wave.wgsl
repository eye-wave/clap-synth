struct V {
    @builtin(position) p: vec4<f32>,
    @location(0) @interpolate(flat) outline: u32,
}

@vertex
fn vs(@location(0) p: vec2<f32>, @builtin(vertex_index) i: u32) -> V {
    return V(vec4(p, 0.0, 1.0), u32(i < 8u));
}

@fragment
fn fs(@location(0) @interpolate(flat) outline: u32) -> @location(0) vec4<f32> {
    if outline == 1u {
        return vec4(0.5, 0.5, 0.55, 1.0);
    }
    return vec4(1.0, 0.5, 0.1, 1.0);
}
