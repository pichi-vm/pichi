// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `Manifest` round-trip + D-07 validation
//! (FORMAT-01..03 / SC5). Public-API only — no `crate::` paths.

use std::collections::BTreeMap;

use pichi_artifact::{
    ConfigDescriptor, Layer, MEDIA_TYPE_PICHI_ARTIFACT_V1, Manifest, PmiDescriptor,
    ScuteAnnotations, ScuteDescriptor,
};

fn chain_annotations() -> BTreeMap<String, String> {
    [
        ("dev.pichi.carapace.verity.algo", "sha256"),
        ("dev.pichi.carapace.verity.data-block-size", "4096"),
        ("dev.pichi.carapace.verity.hash-block-size", "4096"),
        (
            "dev.pichi.carapace.verity.hash",
            "abababababababababababababababababababababababababababababababab",
        ),
        ("org.opencontainers.image.created", "2026-05-06T14:32:00Z"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn appliance_manifest() -> Manifest {
    Manifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: MEDIA_TYPE_PICHI_ARTIFACT_V1.into(),
        config: ConfigDescriptor::canonical(),
        layers: vec![
            Layer::Scute(ScuteDescriptor {
                digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
                size: 4096,
                annotations: ScuteAnnotations {
                    salt: "dead".into(),
                },
            }),
            Layer::Pmi(PmiDescriptor {
                digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .into(),
                size: 8192,
            }),
        ],
        annotations: chain_annotations(),
    }
}

#[test]
fn appliance_round_trip_and_validates() {
    let m = appliance_manifest();
    let bytes = m.to_bytes().expect("serialise");
    let m2 = Manifest::from_reader_validated(bytes.as_slice()).expect("parse + validate");
    assert_eq!(m, m2);
}

#[test]
fn artifact_type_lives_at_top_level_not_in_config() {
    // SC5 explicit assertion: artifactType MUST be top-level (OCI 1.1 pattern),
    // NOT inside config.mediaType (OCI 1.0 legacy).
    let m = appliance_manifest();
    let bytes = m.to_bytes().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v["artifactType"].as_str(),
        Some(MEDIA_TYPE_PICHI_ARTIFACT_V1)
    );
    // The empty config's mediaType is the OCI empty type — NOT the artifact type.
    assert_ne!(
        v["config"]["mediaType"].as_str(),
        Some(MEDIA_TYPE_PICHI_ARTIFACT_V1)
    );
}

#[test]
fn unknown_layer_mediatype_rejected_at_parse() {
    let bad = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": MEDIA_TYPE_PICHI_ARTIFACT_V1,
        "config": ConfigDescriptor::canonical(),
        "layers": [{
            "mediaType": "application/vnd.evil.v1",
            "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "size": 1,
        }],
        "annotations": chain_annotations(),
    });
    let r: Result<Manifest, _> = serde_json::from_value(bad);
    assert!(r.is_err(), "unknown layer mediaType must error");
}
