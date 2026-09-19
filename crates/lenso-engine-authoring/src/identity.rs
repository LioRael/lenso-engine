//! Canonical identities shared by Plugin authoring and catalog consumers.

pub use lenso_plugin_catalog::identity::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../contracts/plugin-identity-v1.conformance.json"
        ))
        .unwrap()
    }

    #[test]
    fn rust_validator_obeys_published_plugin_id_vectors() {
        let vectors = vectors();
        for value in vectors["pluginId"]["valid"].as_array().unwrap() {
            let value = value.as_str().unwrap();
            assert!(
                validate_plugin_id_v1(value).is_ok(),
                "expected valid: {value}"
            );
        }
        for value in vectors["pluginId"]["invalid"].as_array().unwrap() {
            let value = value.as_str().unwrap();
            assert!(
                validate_plugin_id_v1(value).is_err(),
                "expected invalid: {value}"
            );
        }
    }

    #[test]
    fn rust_validator_obeys_published_semver_vectors() {
        let vectors = vectors();
        for value in vectors["version"]["valid"].as_array().unwrap() {
            let value = value.as_str().unwrap();
            assert!(
                validate_release_version(value).is_ok(),
                "expected valid: {value}"
            );
        }
        for value in vectors["version"]["invalid"].as_array().unwrap() {
            let value = value.as_str().unwrap();
            assert!(
                validate_release_version(value).is_err(),
                "expected invalid: {value}"
            );
        }
    }

    #[test]
    fn existing_projects_can_be_opened_with_an_explicit_legacy_classification() {
        assert_eq!(
            classify_existing_plugin_id("uppercase").unwrap(),
            PluginIdVersion::Legacy
        );
        assert_eq!(
            classify_existing_plugin_id("company.uppercase").unwrap(),
            PluginIdVersion::V1
        );
        assert_eq!(
            classify_existing_plugin_id("uppercase-v2").unwrap(),
            PluginIdVersion::Legacy
        );
        assert!(classify_existing_plugin_id("company..uppercase").is_err());
    }
}
