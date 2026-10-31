//! Keep catalog effects aligned with the kernel's actual Cargo features.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

fn kernel_feature_references(value: &toml::Value, references: &mut BTreeSet<String>) {
    match value {
        toml::Value::Table(table) => {
            if let Some(feature) = table.get("kernel_feature") {
                references.insert(feature.as_str().unwrap().to_owned());
            }
            if let Some(features) = table.get("kernel_features") {
                references.extend(
                    features
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|feature| feature.as_str().unwrap().to_owned()),
                );
            }
            for child in table.values() {
                kernel_feature_references(child, references);
            }
        }
        toml::Value::Array(array) => {
            for child in array {
                kernel_feature_references(child, references);
            }
        }
        _ => {}
    }
}

#[test]
fn catalog_kernel_features_exist_in_the_kernel_manifest() {
    let catalog: toml::Value = toml::from_str(include_str!("../../../features.toml")).unwrap();
    let kernel: toml::Value =
        toml::from_str(include_str!("../../../crates/kernel/Cargo.toml")).unwrap();
    let declared = kernel["features"].as_table().unwrap();
    let mut references = BTreeSet::new();
    kernel_feature_references(&catalog, &mut references);
    assert!(!references.is_empty());
    let missing: Vec<_> = references
        .iter()
        .filter(|feature| !declared.contains_key(feature.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "undefined kernel Cargo features: {missing:?}"
    );
}
