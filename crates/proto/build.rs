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
}
