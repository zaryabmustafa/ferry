fn main() {
    for f in ["ferry.rc", "ferry.ico", "ferry.manifest"] {
        println!("cargo:rerun-if-changed=../../assets/brand/{f}");
    }
    embed_resource::compile("../../assets/brand/ferry.rc", embed_resource::NONE).manifest_optional().unwrap();
}
