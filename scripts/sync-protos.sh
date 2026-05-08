#!/usr/bin/env bash
# Re-fetch vendored protobuf definitions from upstream at pinned commits.
#
# Sources:
#   - tronprotocol/java-tron      → proto/tron/      (block / tx / contract / p2p)
#   - streamingfast/proto         → proto/firehose/  (sf.firehose.v2)
#
# Override the pinned refs by env:
#   JAVA_TRON_COMMIT=GreatVoyage-v4.8.1 FIREHOSE_COMMIT=main ./scripts/sync-protos.sh
#
# The script writes the resolved commit SHAs back into proto/README.md so
# the reproducible-source-of-truth lives next to the vendored files.

set -euo pipefail

JAVA_TRON_REPO="https://github.com/tronprotocol/java-tron"
JAVA_TRON_COMMIT="${JAVA_TRON_COMMIT:-master}"

# bstream.v2 service + message definitions live in streamingfast/proto
# (NOT firehose-core, which is the Go implementation; the .proto files
# are tracked in the standalone proto repo). The firehose-core docs link
# points at github.com/streamingfast/bstream, but that's the Go runtime;
# the source-of-truth schema is streamingfast/proto's sf/firehose/v2/.
FIREHOSE_REPO="https://github.com/streamingfast/proto"
FIREHOSE_COMMIT="${FIREHOSE_COMMIT:-main}"

# google/api/annotations.proto + google/api/http.proto. Needed because
# java-tron's api/api.proto uses HTTP-transcoding annotations on its
# gRPC services. We only need the two .proto files, not the rest of
# googleapis — the find filter below picks them out specifically.
GOOGLEAPIS_REPO="https://github.com/googleapis/googleapis"
GOOGLEAPIS_COMMIT="${GOOGLEAPIS_COMMIT:-master}"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROTO_DIR="$ROOT/proto"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# fetch_at_ref REPO_URL DEST REF
#   Shallow-clones REPO_URL into DEST and checks out REF (tag, branch, or
#   commit SHA). Uses `git fetch --depth 1 REF && git checkout FETCH_HEAD`
#   because a shallow clone doesn't materialize tag refs locally; checking
#   out FETCH_HEAD is the portable way after a depth-1 fetch.
fetch_at_ref() {
    local repo="$1" dest="$2" ref="$3"
    git clone --depth 1 "$repo" "$dest" >/dev/null
    ( cd "$dest" \
        && git fetch --depth 1 origin "$ref" \
        && git checkout --quiet FETCH_HEAD )
    # Echo the resolved commit SHA so the caller can record it.
    ( cd "$dest" && git rev-parse HEAD )
}

echo "==> fetching java-tron @ $JAVA_TRON_COMMIT"
JAVA_TRON_SHA=$(fetch_at_ref "$JAVA_TRON_REPO" "$TMP/java-tron" "$JAVA_TRON_COMMIT")
echo "    resolved → $JAVA_TRON_SHA"

# Mirror upstream's directory layout under proto/tron/ so import paths
# like `import "core/Discover.proto";` in Tron.proto resolve at codegen
# time. Flattening into a single dir was a previous design and broke
# imports. The 0-byte stub files under core/tron/ in upstream (block,
# transaction, account, vote, witness, p2p, proposal, delegated_resource)
# are leftover scaffolding — `protoc` rejects empty files, so we drop
# them after the copy. The actual schemas all live in core/Tron.proto.
SRC="$TMP/java-tron/protocol/src/main/protos"
mkdir -p "$PROTO_DIR/tron"
rm -rf "$PROTO_DIR/tron"/*  # full replace each sync
cp -R "$SRC/." "$PROTO_DIR/tron/"
find "$PROTO_DIR/tron" -name "*.proto" -size 0 -delete
echo "    copied $(find "$PROTO_DIR/tron" -name '*.proto' | wc -l | tr -d ' ') .proto files (empty stubs dropped)"

echo "==> fetching firehose-core @ $FIREHOSE_COMMIT"
FIREHOSE_SHA=$(fetch_at_ref "$FIREHOSE_REPO" "$TMP/firehose" "$FIREHOSE_COMMIT")
echo "    resolved → $FIREHOSE_SHA"

mkdir -p "$PROTO_DIR/firehose"
rm -f "$PROTO_DIR/firehose"/*.proto
find "$TMP/firehose" -path '*/sf/firehose/v2/*.proto' -exec cp {} "$PROTO_DIR/firehose/" \;
echo "    copied $(ls "$PROTO_DIR/firehose/" | wc -l | tr -d ' ') .proto files"

echo "==> fetching googleapis @ $GOOGLEAPIS_COMMIT"
GOOGLEAPIS_SHA=$(fetch_at_ref "$GOOGLEAPIS_REPO" "$TMP/googleapis" "$GOOGLEAPIS_COMMIT")
echo "    resolved → $GOOGLEAPIS_SHA"

# Vendor only the two .proto files java-tron's api.proto imports —
# google/api/annotations.proto + google/api/http.proto. The full
# googleapis repo is hundreds of MB; we need ~6 KB.
mkdir -p "$PROTO_DIR/google/api"
rm -f "$PROTO_DIR/google/api"/*.proto
cp "$TMP/googleapis/google/api/annotations.proto" "$PROTO_DIR/google/api/"
cp "$TMP/googleapis/google/api/http.proto" "$PROTO_DIR/google/api/"
echo "    copied $(find "$PROTO_DIR/google/api" -name '*.proto' | wc -l | tr -d ' ') .proto files"

# Stamp the resolved SHAs into proto/README.md so the next reader can
# reproduce this exact vendoring without grepping git history.
README="$PROTO_DIR/README.md"
if [ -f "$README" ]; then
    # Replace any line containing "tronprotocol/java-tron" + "commit `..."
    # patterns. Sed in-place: macOS uses `-i ''`, GNU uses `-i`. Use a
    # tempfile to be portable.
    python3 - "$README" "$JAVA_TRON_SHA" "$FIREHOSE_SHA" "$GOOGLEAPIS_SHA" <<'PY'
import re, sys, pathlib
p = pathlib.Path(sys.argv[1])
java_sha, fh_sha, ga_sha = sys.argv[2], sys.argv[3], sys.argv[4]
text = p.read_text()
text = re.sub(r"(tronprotocol/java-tron[^\n]*?at commit `)[^`]+(`)",
              lambda m: f"{m.group(1)}{java_sha}{m.group(2)}", text)
# Match either `streamingfast/firehose-core` (legacy) or `streamingfast/proto`.
text = re.sub(r"(streamingfast/(firehose-core|proto)[^\n]*?at commit `)[^`]+(`)",
              lambda m: f"{m.group(1)}{fh_sha}{m.group(3)}", text)
text = re.sub(r"(googleapis/googleapis[^\n]*?at commit `)[^`]+(`)",
              lambda m: f"{m.group(1)}{ga_sha}{m.group(2)}", text)
p.write_text(text)
PY
    echo "    stamped SHAs into $README"
fi

echo
echo "==> done."
echo "    java-tron      = $JAVA_TRON_SHA"
echo "    firehose-core  = $FIREHOSE_SHA"
echo "    googleapis     = $GOOGLEAPIS_SHA"
