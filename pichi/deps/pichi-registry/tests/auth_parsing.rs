// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "http-client")]
#![allow(missing_docs)] // Integration tests; not part of public API
//
// Phase 44 D-04 / REGISTRY-06: containers-auth.json + ~/.docker/config.json
// fixture-based tests. Each test isolates fixtures in a fresh tempfile::TempDir
// and injects path overrides via AuthEnv (mirrors src/config.rs::Config::load_from_paths
// over Config::load — never touches the operator's $HOME).

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine;
use oci_client::secrets::RegistryAuth;
use pichi_registry::{AuthEnv, AuthHint, resolve_for_registry};
use tempfile::TempDir;

fn write_at(dir: &Path, rel: &str, body: &str) -> PathBuf {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, body).unwrap();
    p
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

#[test]
fn auth_parses_basic_auth() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    let auth_b64 = b64("alice:secret");
    write_at(
        &xdg,
        "containers/auth.json",
        &format!(r#"{{ "auths": {{ "ghcr.io": {{ "auth": "{auth_b64}" }} }} }}"#),
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        ..AuthEnv::default()
    };
    let auth = resolve_for_registry("ghcr.io", None, &env).unwrap();
    match auth {
        RegistryAuth::Basic(u, p) => {
            assert_eq!(u, "alice");
            assert_eq!(p, "secret");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn auth_parses_identity_token() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    write_at(
        &xdg,
        "containers/auth.json",
        r#"{ "auths": { "registry.example.com": { "identitytoken": "eyJhbGc.dummy.token" } } }"#,
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        ..AuthEnv::default()
    };
    let auth = resolve_for_registry("registry.example.com", None, &env).unwrap();
    match auth {
        RegistryAuth::Bearer(t) => assert_eq!(t, "eyJhbGc.dummy.token"),
        other => panic!("expected Bearer, got {other:?}"),
    }
}

#[test]
fn auth_credsstore_loud_error() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    write_at(
        &xdg,
        "containers/auth.json",
        r#"{ "auths": { "ghcr.io": { "credsStore": "secretservice" } } }"#,
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        ..AuthEnv::default()
    };
    let err = resolve_for_registry("ghcr.io", None, &env).unwrap_err();
    let msg = err.to_string();
    // VERBATIM D-04 wording — the `format!` in auth.rs MUST match this exactly.
    assert!(
        msg.contains(r#"credsStore "secretservice" not supported by pichi"#),
        "missing exact D-04 wording in error: {msg}"
    );
    assert!(
        msg.contains(
            "configure static credentials in pichi's config.toml or remove the credsStore entry"
        ),
        "missing remediation hint in error: {msg}"
    );
}

#[test]
fn auth_credsstore_unrelated_registry_skipped() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    // The auth file declares credsStore for registry-a.io, but we ask for ghcr.io.
    // Per D-04: anonymous query for ghcr.io must succeed (file lookup misses).
    write_at(
        &xdg,
        "containers/auth.json",
        r#"{ "auths": { "registry-a.io": { "credsStore": "secretservice" } } }"#,
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        ..AuthEnv::default()
    };
    let auth = resolve_for_registry("ghcr.io", None, &env).unwrap();
    assert!(
        matches!(auth, RegistryAuth::Anonymous),
        "anonymous pull for unrelated registry must succeed despite credsStore in auth.json"
    );
}

#[test]
fn auth_search_order_xdg_config_wins_over_docker_config() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    let home = tmp.path().join("home");
    let xdg_b64 = b64("xdguser:xdgpass");
    let docker_b64 = b64("dockuser:dockpass");
    write_at(
        &xdg,
        "containers/auth.json",
        &format!(r#"{{ "auths": {{ "ghcr.io": {{ "auth": "{xdg_b64}" }} }} }}"#),
    );
    write_at(
        &home,
        ".docker/config.json",
        &format!(r#"{{ "auths": {{ "ghcr.io": {{ "auth": "{docker_b64}" }} }} }}"#),
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        home: Some(home),
        ..AuthEnv::default()
    };
    let auth = resolve_for_registry("ghcr.io", None, &env).unwrap();
    match auth {
        RegistryAuth::Basic(u, _p) => assert_eq!(
            u, "xdguser",
            "XDG containers/auth.json must win over ~/.docker/config.json"
        ),
        other => panic!("expected Basic, got {other:?}"),
    }
}

#[test]
fn auth_docker_config_fallback() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let docker_b64 = b64("dockuser:dockpass");
    write_at(
        &home,
        ".docker/config.json",
        &format!(r#"{{ "auths": {{ "ghcr.io": {{ "auth": "{docker_b64}" }} }} }}"#),
    );
    let env = AuthEnv {
        home: Some(home),
        ..AuthEnv::default()
    };
    let auth = resolve_for_registry("ghcr.io", None, &env).unwrap();
    match auth {
        RegistryAuth::Basic(u, p) => {
            assert_eq!(u, "dockuser");
            assert_eq!(p, "dockpass");
        }
        other => panic!("expected Basic from docker config, got {other:?}"),
    }
}

#[test]
fn auth_pichi_hint_wins_over_files() {
    let tmp = TempDir::new().unwrap();
    let xdg = tmp.path().join("xdg-config");
    let auth_b64 = b64("file_user:file_pass");
    write_at(
        &xdg,
        "containers/auth.json",
        &format!(r#"{{ "auths": {{ "ghcr.io": {{ "auth": "{auth_b64}" }} }} }}"#),
    );
    let env = AuthEnv {
        xdg_config_home: Some(xdg),
        ..AuthEnv::default()
    };
    let hint = AuthHint {
        identity_token: Some("from_pichi_config".into()),
        ..AuthHint::default()
    };
    let auth = resolve_for_registry("ghcr.io", Some(&hint), &env).unwrap();
    match auth {
        RegistryAuth::Bearer(t) => assert_eq!(
            t, "from_pichi_config",
            "pichi config hint must override containers-auth.json per D-04 order"
        ),
        other => panic!("expected Bearer from hint, got {other:?}"),
    }
}
