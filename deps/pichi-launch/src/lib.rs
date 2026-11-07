// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Typed requests and responses for the bidirectional appliance console.
//! Commands version their own request and response payloads. Transport and
//! filesystem resolution are outside this portable contract.

mod command;
mod finish;
mod input;
mod request;
mod requests;
mod response;
mod start;

pub use command::Command;
pub use finish::Finish;
pub use input::Input;
pub use request::Request;
pub use requests::Requests;
pub use response::Response;
pub use start::Start;

/// Named bidirectional console used by the build appliance.
pub const PORT_NAME: &str = "dev.pichi.build";

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    fn cbor(value: &impl Serialize) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).unwrap();
        bytes
    }

    fn input() -> Input {
        Input {
            r#ref: "refs/heads/main".into(),
            plan: "pichi.yaml".into(),
            lock: "pichi.lock".into(),
            context: ".".into(),
            location: None,
        }
    }

    #[test]
    fn start_body_encodes_as_null_and_rejects_non_null_values() {
        let start = Start::default();

        assert_eq!(cbor(&start), b"\xf6");
        assert_eq!(serde_json::to_string(&start).unwrap(), "null");
        assert_eq!(
            ciborium::from_reader::<Start, _>(&b"\xf6"[..]).unwrap(),
            start
        );
        assert_eq!(serde_json::from_str::<Start>("null").unwrap(), start);
        for invalid in [json!(false), json!(0), json!(""), json!([]), json!({})] {
            assert!(ciborium::from_reader::<Start, _>(&cbor(&invalid)[..]).is_err());
            assert!(serde_json::from_value::<Start>(invalid).is_err());
        }

        // Ciborium presents undefined as None too; serialization normalizes it.
        let decoded: Start = ciborium::from_reader(&b"\xf7"[..]).unwrap();
        assert_eq!(cbor(&decoded), b"\xf6");
    }

    #[test]
    fn start_request_has_exact_wire_bytes_and_server_dispatch() {
        let request = Request::from(Start::default());
        let expected = b"\x83\x65start\x01\xf6";
        assert_eq!(cbor(&request), expected);
        assert_eq!(cbor(&Requests::Start(request)), expected);
        assert_eq!(
            ciborium::from_reader::<Request<Start>, _>(&expected[..]).unwrap(),
            request
        );
        assert_eq!(
            ciborium::from_reader::<Requests, _>(&expected[..]).unwrap(),
            Requests::Start(request)
        );
        assert_eq!(
            serde_json::from_str::<Requests>(r#"["start", 1, null]"#).unwrap(),
            Requests::Start(request)
        );
    }

    #[test]
    fn requests_reject_wrong_commands_versions_bodies_and_lengths() {
        for value in [
            json!([]),
            json!(["start"]),
            json!(["start", 1]),
            json!(["other", 1, null]),
            json!(["start", 0, null]),
            json!(["start", 2, null]),
            json!(["start", 65536, null]),
            json!(["start", -1, null]),
            json!(["start", 1.0, null]),
            json!(["start", "1", null]),
            json!(["start", true, null]),
            json!([1, 1, null]),
            json!(["start", 1, {}]),
            json!(["start", 1, []]),
            json!(["start", 1, false]),
            json!({"v": 1, "op": "build-input"}),
        ] {
            let bytes = cbor(&value);
            assert!(
                ciborium::from_reader::<Request<Start>, _>(&bytes[..]).is_err(),
                "{value}"
            );
            assert!(
                ciborium::from_reader::<Requests, _>(&bytes[..]).is_err(),
                "{value}"
            );
        }
        let bytes = cbor(&Request(Start::default()));
        for end in 0..bytes.len() {
            assert!(ciborium::from_reader::<Requests, _>(&bytes[..end]).is_err());
        }
    }

    #[test]
    fn request_dispatch_rejects_extra_elements() {
        let value = json!(["start", 1, null, null]);
        assert!(ciborium::from_reader::<Requests, _>(&cbor(&value)[..]).is_err());
        assert!(serde_json::from_value::<Requests>(value).is_err());
        // Ciborium delegates tuple length checks to its visitor. The untagged
        // server dispatcher buffers the whole request and enforces the length.
        let indefinite = b"\x9f\x65start\x01\xf6\xf6\xff";
        assert!(ciborium::from_reader::<Requests, _>(&indefinite[..]).is_err());
    }

    #[test]
    fn response_status_selects_the_body_and_has_two_byte_overhead() {
        let body = input();
        let response: Response<<Start as Command>::Response> = Response::Ok(body.clone());
        let mut expected = vec![0x82, 0xf5];
        expected.extend(cbor(&body));
        assert_eq!(cbor(&response), expected);
        assert_eq!(
            ciborium::from_reader::<Response<Input>, _>(&expected[..]).unwrap(),
            response
        );
        let failure = Response::<Input>::Err("failed".into());
        let expected = b"\x82\xf4\x66failed";
        assert_eq!(cbor(&failure), expected);
        assert_eq!(
            ciborium::from_reader::<Response<Input>, _>(&expected[..]).unwrap(),
            failure
        );
        // Even identical success/error payload types are distinguished by status.
        for response in [
            Response::Ok("same".to_string()),
            Response::Err("same".to_string()),
            Response::Err(String::new()),
            Response::Err("host\0message".to_string()),
        ] {
            assert_eq!(
                ciborium::from_reader::<Response<String>, _>(&cbor(&response)[..]).unwrap(),
                response
            );
        }
    }

    #[test]
    fn responses_reject_malformed_envelopes() {
        for value in [
            json!([]),
            json!([true]),
            json!([false]),
            json!([true, {}, 0]),
            json!([true, input(), 0]),
            json!([false, "error", 0]),
            json!([1, {}]),
            json!(["ok", {}]),
            json!([true, "error"]),
            json!([false, {}]),
            json!([false, null]),
            json!({"v": 1, "ok": false, "error": "old envelope"}),
        ] {
            assert!(
                ciborium::from_reader::<Response<Input>, _>(&cbor(&value)[..]).is_err(),
                "{value}"
            );
        }
        let bytes = cbor(&Response::Ok(input()));
        for end in 0..bytes.len() {
            assert!(ciborium::from_reader::<Response<Input>, _>(&bytes[..end]).is_err());
        }
    }

    #[test]
    fn input_derives_check_wire_shape() {
        let mut value = serde_json::to_value(input()).unwrap();
        assert_eq!(
            value,
            json!({"ref":"refs/heads/main", "plan":"pichi.yaml", "lock":"pichi.lock", "context":"."})
        );
        for name in ["ref", "plan", "lock", "context"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(name);
            assert!(
                ciborium::from_reader::<Input, _>(&cbor(&missing)[..]).is_err(),
                "{name}"
            );
        }
        for name in ["ref", "plan", "lock", "context", "location"] {
            for invalid in [json!(1), json!(false), json!({})] {
                let mut malformed = value.clone();
                malformed[name] = invalid;
                assert!(
                    ciborium::from_reader::<Input, _>(&cbor(&malformed)[..]).is_err(),
                    "{malformed}"
                );
            }
        }
        for name in ["ref", "plan", "lock", "context"] {
            let mut malformed = value.clone();
            malformed[name] = json!(null);
            assert!(ciborium::from_reader::<Input, _>(&cbor(&malformed)[..]).is_err());
        }
        value["location"] = json!(null);
        let decoded: Input = ciborium::from_reader(&cbor(&value)[..]).unwrap();
        assert_eq!(decoded, input());
        assert_eq!(cbor(&decoded), cbor(&input()));

        value["location"] = json!("https://example.test/repo");
        let decoded: Input = ciborium::from_reader(&cbor(&value)[..]).unwrap();
        assert_eq!(
            decoded.location.as_deref(),
            Some("https://example.test/repo")
        );
        value["unknown"] = json!(true);
        assert!(ciborium::from_reader::<Input, _>(&cbor(&value)[..]).is_err());

        let mut fields: ciborium::Value = ciborium::from_reader(&cbor(&input())[..]).unwrap();
        let ciborium::Value::Map(entries) = &mut fields else {
            unreachable!()
        };
        entries.push(entries[0].clone());
        assert!(ciborium::from_reader::<Input, _>(&cbor(&fields)[..]).is_err());
    }

    #[test]
    fn response_converts_to_and_from_standard_result() {
        for result in [Ok(input()), Err("failure".to_string())] {
            assert_eq!(Result::from(Response::from(result.clone())), result);
        }
    }

    #[test]
    fn serde_bounds_allow_borrowed_and_one_way_request_bodies() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Borrowed<'a>(&'a str);
        impl Command for Borrowed<'_> {
            const NAME: &'static str = "borrowed";
            const VERSION: u16 = 2;
            type Response = ();
        }
        #[derive(Serialize)]
        struct Outgoing;
        impl Command for Outgoing {
            const NAME: &'static str = "outgoing";
            const VERSION: u16 = 256;
            type Response = ();
        }
        let request: Request<Borrowed<'_>> =
            serde_json::from_str(r#"["borrowed",2,"value"]"#).unwrap();
        assert_eq!(request.0, Borrowed("value"));
        let response: Response<&str> = serde_json::from_str(r#"[true,"value"]"#).unwrap();
        assert_eq!(response, Response::Ok("value"));

        assert_eq!(
            serde_json::to_value(Request(Outgoing)).unwrap(),
            json!(["outgoing", 256, null])
        );
    }

    #[test]
    fn finish_dispatch_reports_success_or_failure() {
        for error in [None, Some("apt exited with status 100".to_string())] {
            let request = Request(Finish(error.clone()));
            let bytes = cbor(&request);
            assert_eq!(bytes, cbor(&("finish", 1, &error)));
            assert_eq!(
                ciborium::from_reader::<Requests, _>(&bytes[..]).unwrap(),
                Requests::Finish(request)
            );
        }
        for value in [
            json!(["finish", 2, null]),
            json!(["finish", 1, false]),
            json!(["finish", 1, {}]),
            json!(["finish", 1, null, null]),
            json!(["build", 1, null]),
        ] {
            assert!(ciborium::from_reader::<Requests, _>(&cbor(&value)[..]).is_err());
        }
    }
}
