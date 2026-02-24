use std::collections::HashMap;

use risc0_build::{DockerOptionsBuilder, GuestOptions, GuestOptionsBuilder};

fn main() {
    let guest_opts = guest_options();
    let opts =
        HashMap::from([("vprogs-zk-risc0-guest", guest_opts)]);
    risc0_build::embed_methods_with_options(opts);
}

fn guest_options() -> GuestOptions {
    let mut builder = GuestOptionsBuilder::default();
    if std::env::var("RISC0_USE_DOCKER").is_ok() {
        let docker_opts = DockerOptionsBuilder::default()
            .root_dir("../../")
            .build()
            .unwrap();
        builder.use_docker(docker_opts);
    }
    builder.build().unwrap()
}
