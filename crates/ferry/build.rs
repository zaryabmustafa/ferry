fn main() {
    println!("cargo:rerun-if-changed=../../assets/brand/ferry.ico");
    embed_resource::compile("../../assets/brand/ferry.rc", embed_resource::NONE).manifest_optional().unwrap();
}
