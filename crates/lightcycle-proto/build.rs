//! Build script: compile vendored protos under `../../proto/` to Rust types.
//!
//! Four proto trees are compiled, with different client/server requirements:
//!
//! - **`proto/tron/`** — `tronprotocol/java-tron`'s wire format and the
//!   `Wallet` / `WalletSolidity` gRPC services. We are a CLIENT of those
//!   services (we send queries to a running java-tron), so codegen produces
//!   client stubs only. The message types themselves are used everywhere
//!   the codec / source / relayer crates handle blocks and transactions.
//!   Include path = `proto/tron/` plus `proto/google/api/`'s parent so
//!   `core/Discover.proto` and `google/api/annotations.proto` both resolve.
//!
//! - **`proto/firehose/`** — `streamingfast/proto`'s `sf.firehose.v2`
//!   `Stream` / `Fetch` / `EndpointInfo` services. We SERVE this protocol
//!   (downstream consumers like Substreams subscribe to us), so we want
//!   the server stubs. Client stubs are also generated for self-tests
//!   that round-trip through the same surface.
//!
//! - **`proto/sf/`** — lightcycle-authored `sf.tron.type.v1.Block` and
//!   friends. The chain-specific block payload that ships on Firehose
//!   `Response.block`. Pure message types; no services, no client/server
//!   stubs.
//!
//! - **`proto/google/api/`** — only `annotations.proto` + `http.proto`,
//!   vendored from googleapis/googleapis. Required by tron/api/api.proto
//!   for HTTP-transcoding hints on the gRPC services. Two files only;
//!   the rest of googleapis isn't a dependency.
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
    let sf_dir = proto_root.join("sf");

    if !tron_dir.exists() || !firehose_dir.exists() {
        println!(
            "cargo:warning=proto/{{tron,firehose}}/ missing — run scripts/sync-protos.sh"
        );
        return Ok(());
    }

    // tron/api/ now compiles too — google/api/{annotations,http}.proto
    // are vendored under proto/google/api/. The Wallet + WalletSolidity
    // gRPC client stubs land in lightcycle_proto::tron::api.
    let tron_files = collect_protos(&tron_dir, &[])?;
    let firehose_files = collect_protos(&firehose_dir, &[])?;
    let sf_files = if sf_dir.exists() {
        collect_protos(&sf_dir, &[])?
    } else {
        Vec::new()
    };

    // Include paths:
    //   1. The proto-tree's own root (`proto/tron/`, `proto/firehose/`,
    //      `proto/sf/`) so internal cross-imports
    //      (`import "core/Discover.proto"`) resolve.
    //   2. `proto_root` itself, so `import "google/api/annotations.proto"`
    //      resolves to our vendored copy at proto/google/api/...
    //   3. `/usr/include` IF it has the well-known google/protobuf/*.proto
    //      (Debian-via-apt installs them there via libprotobuf-dev). On
    //      macOS-via-brew, protoc finds them implicitly without an
    //      explicit include — the IF guards skip adding when not needed.
    let mut tron_includes = vec![tron_dir.clone(), proto_root.clone()];
    let mut firehose_includes = vec![firehose_dir.clone(), proto_root.clone()];
    let mut sf_includes = vec![sf_dir.clone(), proto_root.clone()];
    let system_include = Path::new("/usr/include");
    if system_include.join("google/protobuf/any.proto").exists() {
        tron_includes.push(system_include.to_path_buf());
        firehose_includes.push(system_include.to_path_buf());
        sf_includes.push(system_include.to_path_buf());
    }

    println!(
        "cargo:warning=lightcycle-proto: {} tron + {} firehose + {} sf .proto files",
        tron_files.len(),
        firehose_files.len(),
        sf_files.len()
    );

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&tron_files, &tron_includes)?;

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&firehose_files, &firehose_includes)?;

    // sf protos are pure messages (no services), so no client/server
    // stubs. tonic_build still picks up the prost message codegen.
    if !sf_files.is_empty() {
        tonic_build::configure()
            .build_server(false)
            .build_client(false)
            .compile_protos(&sf_files, &sf_includes)?;
    }

    println!("cargo:rerun-if-changed=../../proto");
    Ok(())
}
