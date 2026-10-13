// WGSL -> SPIR-V at build time, so no glslc / Vulkan SDK needed.
fn main() {
    println!("cargo:rerun-if-changed=src/tri.wgsl");
    let m =
        naga::front::wgsl::parse_str(&std::fs::read_to_string("src/tri.wgsl").unwrap()).unwrap();
    let i = naga::valid::Validator::new(Default::default(), naga::valid::Capabilities::all())
        .validate(&m)
        .unwrap();
    let w = naga::back::spv::write_vec(&m, &i, &Default::default(), None).unwrap();
    let b: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).collect();
    std::fs::write(format!("{}/tri.spv", std::env::var("OUT_DIR").unwrap()), b).unwrap();
}
