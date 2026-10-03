// SPDX-License-Identifier: MIT
//! Generates the `coaxhub.station.v1` prost messages and tonic services from
//! `proto` into `OUT_DIR` (never into the source tree).

use std::path::PathBuf;

const PROTO_FILES: &[&str] = &[
    "coaxhub/station/v1/common.proto",
    "coaxhub/station/v1/control.proto",
    "coaxhub/station/v1/enrollment.proto",
    "coaxhub/station/v1/gateway.proto",
    "coaxhub/station/v1/measure.proto",
    "coaxhub/station/v1/media.proto",
    "coaxhub/station/v1/status.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("../../proto");
    println!("cargo:rerun-if-changed={}", root.display());
    let files: Vec<PathBuf> = PROTO_FILES.iter().map(|f| root.join(f)).collect();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    // Comments in the .proto files become rustdoc on the generated items
    // (prost-build's default). Clients and servers for all three services
    // (StationGateway, Enrollment, StationControl) are generated.
    //
    // `build_transport(false)`: the transport convenience constructor
    // `XClient::connect(endpoint)` would clash with the `Connect` RPC method
    // of StationGatewayClient. Build clients with `XClient::new(channel)`.
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .build_transport(false)
        .compile_protos(&files, &[root])?;
    Ok(())
}
