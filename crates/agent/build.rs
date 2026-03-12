use ttrpc_codegen::Codegen;
use ttrpc_codegen::Customize;

fn main() {
    let protos = vec!["../../proto/agent.proto"];

    Codegen::new()
        .out_dir("src/generated")
        .inputs(&protos)
        .include("../../proto")
        .rust_protobuf()
        .customize(Customize {
            async_all: true,
            ..Default::default()
        })
        .run()
        .expect("failed to generate ttrpc code from protos");

    // Strip removed `box_pointers` lint from generated code (removed in Rust 1.86+)
    for entry in std::fs::read_dir("src/generated").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let content = std::fs::read_to_string(&path).unwrap();
            if content.contains("box_pointers") {
                let fixed = content.replace("#![allow(box_pointers)]\n", "");
                std::fs::write(&path, fixed).unwrap();
            }
        }
    }
}
