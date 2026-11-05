// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "http-client")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use conglobate::artifact::Digest;
use pichi_registry::{AuthEnv, AuthHint, HttpRegistry, Registry as _, RegistryEntry};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio_util::io::StreamReader;

#[tokio::test]
async fn blob_download_refreshes_rejected_token_once() {
    for streaming in [false, true] {
        for final_status in [200, 401, 403] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let challenge = format!(
                "WWW-Authenticate: Bearer realm=\"http://{address}/token\",service=\"test\"\r\n"
            );
            let digest =
                Digest::Sha256(<sha2::Sha256 as sha2::Digest>::digest(b"blob bytes").into());
            let blob_path = format!("/v2/test/blob/blobs/{digest}");
            let counts = Arc::new(Mutex::new((0, 0)));
            let observed = Arc::clone(&counts);
            let server = tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    socket.read_line(&mut request).await.unwrap();
                    let path = request.split_whitespace().nth(1).unwrap();
                    let mut authorization = None;
                    loop {
                        let mut line = String::new();
                        assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                        if line == "\r\n" {
                            break;
                        }
                        let (name, value) = line.split_once(':').unwrap();
                        if name.eq_ignore_ascii_case("authorization") {
                            authorization = Some(value.trim().to_owned());
                        }
                    }
                    let (status, body, headers) = {
                        let mut counts = observed.lock().unwrap();
                        if path == "/v2/" {
                            (401, String::new(), challenge.clone())
                        } else if path.starts_with("/token?") {
                            assert_eq!(authorization.as_deref(), Some("Basic YWxpY2U6c2VjcmV0"));
                            counts.0 += 1;
                            (
                                200,
                                format!(r#"{{"token":"token-{}"}}"#, counts.0),
                                String::new(),
                            )
                        } else {
                            assert_eq!(path, blob_path);
                            counts.1 += 1;
                            assert_eq!(authorization, Some(format!("Bearer token-{}", counts.0)));
                            assert_eq!(counts.0, counts.1, "retry must fetch a new token");
                            let status = if counts.1 == 1 && final_status != 403 {
                                401
                            } else {
                                final_status
                            };
                            let body = if status == 200 {
                                "blob bytes"
                            } else {
                                "denied"
                            };
                            (status, body.into(), challenge.clone())
                        }
                    };
                    let header = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
                        body.len()
                    );
                    socket.get_mut().write_all(header.as_bytes()).await.unwrap();
                    socket.get_mut().write_all(body.as_bytes()).await.unwrap();
                    socket.get_mut().shutdown().await.unwrap();
                }
            });
            let registry = HttpRegistry::new(
                vec![RegistryEntry {
                    prefix: address.clone(),
                    insecure: true,
                    auth_hint: Some(AuthHint {
                        username: Some("alice".into()),
                        password: Some("secret".into()),
                        ..AuthHint::default()
                    }),
                }],
                AuthEnv::default(),
            );
            let mut data = Vec::new();
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                if streaming {
                    match registry
                        .pull_blob_stream(&address, "test/blob", &digest, 10)
                        .await
                    {
                        Ok(stream) => StreamReader::new(stream)
                            .read_to_end(&mut data)
                            .await
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    registry
                        .pull_blob(&address, "test/blob", &digest, 10, &mut data)
                        .await
                        .map_err(|error| error.to_string())
                }
            })
            .await;
            server.abort();
            let error = server.await.unwrap_err();
            assert!(error.is_cancelled(), "registry fixture failed: {error}");
            let result = result.expect("blob request must finish without a retry loop");
            if final_status == 200 {
                result.unwrap();
                assert_eq!(data, b"blob bytes");
            } else {
                assert!(result.is_err());
                assert!(
                    data.is_empty(),
                    "rejected responses must not reach the sink"
                );
            }
            let expected = if final_status == 403 { 1 } else { 2 };
            assert_eq!(*counts.lock().unwrap(), (expected, expected));
        }
    }
}
