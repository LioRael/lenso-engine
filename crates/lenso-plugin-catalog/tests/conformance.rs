//! Keep the existing Marketplace cross-language vector unchanged during extraction.

use ed25519_dalek::SigningKey;
use lenso_plugin_catalog::{Snapshot, Trust, sign, verify};
use std::collections::BTreeMap;

#[test]
fn existing_wire_vector_is_verified_and_reproduced_byte_for_byte() {
    let vector: serde_json::Value = serde_json::from_str(include_str!("conformance.json")).unwrap();
    let key = SigningKey::from_bytes(&[17; 32]);
    assert_eq!(
        hex::encode(key.verifying_key().as_bytes()),
        vector["public_key_hex"]
    );
    let trust = Trust {
        catalog_id: "conformance-only".into(),
        keys: BTreeMap::from([("test-key".into(), key.verifying_key())]),
    };
    let verified = verify(
        &serde_json::to_vec(&vector["envelope"]).unwrap(),
        &trust,
        None,
        150,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(verified.snapshot()).unwrap(),
        vector["expected_payload"]
    );
    let snapshot = Snapshot::new("conformance-only".into(), 1, 100, 200, vec![]);
    let signed = sign(&snapshot, "test-key", &key).unwrap();
    assert_eq!(signed, serde_json::to_vec(&vector["envelope"]).unwrap());
}

#[test]
fn expired_browsing_preserves_integrity_and_installation_freshness() {
    use lenso_plugin_catalog::verify_for_browse;
    let key = SigningKey::from_bytes(&[17; 32]);
    let trust = Trust {
        catalog_id: "browse".into(),
        keys: BTreeMap::from([("key".into(), key.verifying_key())]),
    };
    let bytes = sign(
        &Snapshot::new("browse".into(), 2, 100, 200, vec![]),
        "key",
        &key,
    )
    .unwrap();
    let current = verify_for_browse(&bytes, &trust, None, 199).unwrap();
    assert!(!current.is_stale(199));
    let expired = verify_for_browse(&bytes, &trust, None, 200).unwrap();
    assert!(expired.is_stale(200));
    assert!(verify(&bytes, &trust, None, 200).is_err());
    assert!(verify_for_browse(&bytes, &trust, None, 99).is_err());
    let older = sign(
        &Snapshot::new("browse".into(), 1, 100, 200, vec![]),
        "key",
        &key,
    )
    .unwrap();
    assert!(verify_for_browse(&older, &trust, Some(expired.checkpoint()), 201).is_err());
    let changed = sign(
        &Snapshot::new("browse".into(), 2, 100, 201, vec![]),
        "key",
        &key,
    )
    .unwrap();
    assert!(verify_for_browse(&changed, &trust, Some(expired.checkpoint()), 202).is_err());
    let forged = sign(
        &Snapshot::new("browse".into(), 3, 100, 200, vec![]),
        "key",
        &SigningKey::from_bytes(&[18; 32]),
    )
    .unwrap();
    assert!(verify_for_browse(&forged, &trust, None, 201).is_err());
}
