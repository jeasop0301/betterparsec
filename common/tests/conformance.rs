//! Protocol conformance kit (G032). Loads the shared canonical wire vectors and
//! asserts the Rust encoders produce byte-identical output. The TypeScript
//! conformance test (`tests/conformance.test.mjs`) loads the SAME file, so any
//! divergence between the two languages — or a future client port — fails here.

use common::desktop_control::clipboard::encode_offer;
use common::desktop_control::cursor::{CursorOwner, encode_authority};
use common::desktop_control::display::{ModeRequest, encode_mode_request};
use common::desktop_control::file_transfer::encode_cancel;
use common::desktop_control::privacy::{
    PrivacyRequest, PrivacyState, encode_request as encode_privacy_request, encode_state,
};
use common::desktop_control::{
    ControlDomain, ControlGeneration, DowngradeReason, TransitionStatus, encode_envelope,
};
use common::media_caps::HdrMetadata;

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd-length hex: {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    v.get(key).unwrap_or_else(|| panic!("missing field {key}"))
}

fn u64_field(v: &serde_json::Value, key: &str) -> u64 {
    field(v, key)
        .as_u64()
        .unwrap_or_else(|| panic!("field {key} not a u64"))
}

#[test]
fn wire_vectors_are_byte_identical() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tests/conformance/wire_vectors.json"
    );
    let raw = std::fs::read_to_string(path).expect("read conformance fixtures");
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("parse fixtures");
    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty());

    for vector in vectors {
        let format = vector["format"].as_str().expect("format");
        let expected = hex_to_bytes(vector["expected_hex"].as_str().expect("expected_hex"));
        let name = vector["name"].as_str().unwrap_or(format);

        let actual: Vec<u8> = match format {
            "control_envelope" => {
                let domain = ControlDomain::from_u8(u64_field(vector, "domain") as u8)
                    .expect("known domain");
                let payload =
                    hex_to_bytes(field(vector, "payload_hex").as_str().expect("payload_hex"));
                encode_envelope(
                    domain,
                    u64_field(vector, "kind") as u16,
                    ControlGeneration(u64_field(vector, "generation") as u32),
                    &payload,
                )
                .expect("payload within cap")
            }
            "display_mode_request" => encode_mode_request(ModeRequest {
                output_id: u64_field(vector, "output_id") as u16,
                width: u64_field(vector, "width") as u16,
                height: u64_field(vector, "height") as u16,
                refresh_mhz: u64_field(vector, "refresh_mhz") as u32,
            })
            .to_vec(),
            "privacy_state" => {
                let status = match field(vector, "status").as_str().expect("status") {
                    "applied" => TransitionStatus::Applied,
                    "downgraded" => TransitionStatus::Downgraded,
                    "rejected" => TransitionStatus::Rejected,
                    other => panic!("unknown status {other}"),
                };
                encode_state(PrivacyState {
                    active: field(vector, "active").as_bool().expect("active"),
                    protections_effective: u64_field(vector, "protections") as u8,
                    status,
                    reason: DowngradeReason::from_u8(u64_field(vector, "reason") as u8),
                })
                .to_vec()
            }
            "hdr_metadata" => HdrMetadata {
                max_cll: u64_field(vector, "max_cll") as u16,
                max_fall: u64_field(vector, "max_fall") as u16,
                max_luminance: u64_field(vector, "max_luminance") as u32,
                min_luminance: u64_field(vector, "min_luminance") as u32,
            }
            .encode()
            .to_vec(),
            "cursor_authority" => {
                let owner = match field(vector, "owner").as_str().expect("owner") {
                    "host" => CursorOwner::Host,
                    "client" => CursorOwner::Client,
                    other => panic!("unknown cursor owner {other}"),
                };
                encode_authority(owner).to_vec()
            }
            "clipboard_offer" => encode_offer(u64_field(vector, "kinds") as u8).to_vec(),
            "privacy_request" => encode_privacy_request(PrivacyRequest {
                enable: field(vector, "enable").as_bool().expect("enable"),
                protections: u64_field(vector, "protections") as u8,
            })
            .to_vec(),
            "file_cancel" => encode_cancel(
                u64_field(vector, "transfer_id") as u32,
                DowngradeReason::from_u8(u64_field(vector, "reason") as u8),
            )
            .to_vec(),
            other => panic!("unknown conformance format {other}"),
        };

        assert_eq!(actual, expected, "conformance vector mismatch: {name}");
    }
}
