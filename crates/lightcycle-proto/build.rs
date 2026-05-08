//! Build script: compile vendored protos under `../../proto/` to Rust types.
//!
//! Two proto trees are compiled, with different client/server requirements:
//!
//! - **`proto/tron/`** — `tronprotocol/java-tron`'s wire format and the
//!   `Wallet` / `WalletSolidity` gRPC services. We are a CLIENT of those
//!   services (we send queries to a running java-tron), so codegen produces
//!   client stubs only. The message types themselves are used everywhere
//!   the codec / source / relayer crates handle blocks and transactions.
//!   Include path = `proto/tron/` so that
//!   `import "core/Discover.proto"` etc. resolve.
//!
//! - **`proto/firehose/`** — `streamingfast/proto`'s `sf.firehose.v2`
//!   `Stream` / `Fetch` / `EndpointInfo` services. We SERVE this protocol
//!   (downstream consumers like Substreams subscribe to us), so we want
//!   the server stubs. Client stubs are also generated for self-tests
//!   that round-trip through the same surface.
//!
//! Empty `.proto` files (java-tron has a handful of zero-byte stubs in
//! `core/tron/`) are filtered before being passed to `protoc`, since
//! protoc rejects them. The sync-protos.sh script also drops them.

use std::path::{Path, PathBuf};

fn collect_protos(root: &Path, skip_dirs: &[&str]) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|name| skip_dirs.contains(&name));
                if !skip {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "proto")
                && entry.metadata()?.len() > 0
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = Path::new("../../proto").canonicalize()?;
    let tron_dir = proto_root.join("tron");
    let firehose_dir = proto_root.join("firehose");

    if !tron_dir.exists() || !firehose_dir.exists() {
        println!(
            "cargo:warning=proto/{{tron,firehose}}/ missing — run scripts/sync-protos.sh"
        );
        return Ok(());
    }

    // Skip `tron/api/` — those service definitions (`Wallet`,
    // `WalletSolidity`) import `google/api/annotations.proto` for
    // HTTP-transcoding hints, which aren't shipped with `protoc` and
    // would require vendoring googleapis just for two files. lightcycle
    // talks to java-tron's HTTP /wallet/* endpoints (per ARCHITECTURE.md
    // RPC fallback design), not the gRPC service. If a future need wants
    // the gRPC client stubs, vendor `google/api/{annotations,http}.proto`
    // and remove this skip.
    let tron_files = collect_protos(&tron_dir, &["api"])?;
    let firehose_files = collect_protos(&firehose_dir, &[])?;

    println!(
        "cargo:warning=lightcycle-proto: {} tron + {} firehose .proto files",
        tron_files.len(),
        firehose_files.len()
    );

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&tron_files, &[tron_dir])?;

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&firehose_files, &[firehose_dir])?;

    println!("cargo:rerun-if-changed=../../proto");
    Ok(())
}
