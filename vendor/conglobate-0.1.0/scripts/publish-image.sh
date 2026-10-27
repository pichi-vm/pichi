#!/usr/bin/env bash
# SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
# SPDX-License-Identifier: Apache-2.0

# Publish the per-arch build-image PMIs as a single multi-arch OCI index that
# `pichi pull` can consume. The manifests + index are hand-built to match
# pichi's artifact schema exactly (see pichi-artifact: artifactType
# application/vnd.pichi.artifact.v1+json, OCI 1.1 empty config, one pmi.v1
# layer, the three dev.pichi.carapace.verity.* annotations, and an index whose
# entries carry platform.os=pichi). Pushed with oras (must be logged in).
#
# Inputs: REPO and TAG env; one "arch=/path/boot.pmi" argument per architecture
# (arch is the OCI platform.architecture, i.e. amd64 / arm64).
set -euo pipefail

REPO="${REPO:?set REPO, e.g. ghcr.io/pichi-vm/conglobate}"
TAG="${TAG:?set TAG}"
REQUIREMENTS_YAML="${REQUIREMENTS_YAML:-$(cd "$(dirname "$0")/.." && pwd)/image/requirements.yaml}"

ARTIFACT_TYPE="application/vnd.pichi.artifact.v1+json"
PMI_MT="application/vnd.pichi.pmi.v1"
REQ_MT="application/vnd.pichi.requirements.v1+yaml"
EMPTY_MT="application/vnd.oci.empty.v1+json"
EMPTY_DIGEST="sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
MANIFEST_MT="application/vnd.oci.image.manifest.v1+json"
INDEX_MT="application/vnd.oci.image.index.v1+json"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Shared OCI 1.1 empty-config blob ({} -> EMPTY_DIGEST, size 2).
printf '{}' >"$tmp/empty"
oras blob push "$REPO" "$tmp/empty" >/dev/null

# Shared requirements.yaml layer (BUILD.md §7) — the build image's launch
# contract, identical across architectures.
req_digest="sha256:$(sha256sum "$REQUIREMENTS_YAML" | cut -d' ' -f1)"
req_size=$(wc -c <"$REQUIREMENTS_YAML")
oras blob push "$REPO" "$REQUIREMENTS_YAML" >/dev/null

entries=()
for spec in "$@"; do
	arch="${spec%%=*}"
	pmi="${spec#*=}"
	pmi_digest="sha256:$(sha256sum "$pmi" | cut -d' ' -f1)"
	pmi_size=$(wc -c <"$pmi")

	oras blob push "$REPO" "$pmi" >/dev/null

	cat >"$tmp/manifest-$arch.json" <<EOF
{
  "schemaVersion": 2,
  "mediaType": "$MANIFEST_MT",
  "artifactType": "$ARTIFACT_TYPE",
  "config": {"mediaType": "$EMPTY_MT", "digest": "$EMPTY_DIGEST", "size": 2, "data": "e30="},
  "layers": [
    {"mediaType": "$PMI_MT", "digest": "$pmi_digest", "size": $pmi_size},
    {"mediaType": "$REQ_MT", "digest": "$req_digest", "size": $req_size}
  ],
  "annotations": {
    "dev.pichi.carapace.verity.algo": "sha256",
    "dev.pichi.carapace.verity.data-block-size": "4096",
    "dev.pichi.carapace.verity.hash-block-size": "4096"
  }
}
EOF
	m_digest="sha256:$(sha256sum "$tmp/manifest-$arch.json" | cut -d' ' -f1)"
	m_size=$(wc -c <"$tmp/manifest-$arch.json")

	# Push the per-arch manifest addressed by digest — it is an index child,
	# not a user-facing tag.
	oras manifest push "$REPO@$m_digest" --media-type "$MANIFEST_MT" \
		"$tmp/manifest-$arch.json" >/dev/null

	entries+=("{\"mediaType\": \"$MANIFEST_MT\", \"digest\": \"$m_digest\", \"size\": $m_size, \"artifactType\": \"$ARTIFACT_TYPE\", \"platform\": {\"os\": \"pichi\", \"architecture\": \"$arch\"}}")
done

manifests=$(IFS=,; echo "${entries[*]}")
cat >"$tmp/index.json" <<EOF
{
  "schemaVersion": 2,
  "mediaType": "$INDEX_MT",
  "manifests": [$manifests]
}
EOF
oras manifest push "$REPO:$TAG" --media-type "$INDEX_MT" "$tmp/index.json"
echo ">>> published $REPO:$TAG (multi-arch pichi build image)"
