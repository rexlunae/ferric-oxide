//! Enabled CPU variants, independent of the single active build target.
//!
//! The catalog owns defaults and baseline dependencies. This projection supplies
//! the build CLI with target/level pairs; it does not assemble a combined image.

use crate::{model::Resolved, schema::Schema, value::Value};

/// A custom catalog lacks a baseline/image choice required for variant planning.
#[derive(Debug, thiserror::Error)]
#[error("CPU variant planning requires the string-valued catalog option {option}")]
pub(crate) struct VariantError {
    option: &'static str,
}

#[derive(serde::Serialize)]
/// One enabled CPU image variant and its separately configured minimum baseline.
pub(crate) struct Variant {
    /// Redox target architecture.
    pub(crate) arch: &'static str,
    /// Compiler/CPU level of this independently built image.
    pub(crate) level: &'static str,
    /// Saved compatibility floor for the architecture.
    pub(crate) minimum: String,
    /// Base image profile shared by the selected variants.
    pub(crate) image: String,
}

/// Projects enabled checkboxes after their minimum-baseline dependencies apply.
///
/// # Errors
///
/// Returns [`VariantError`] when a custom catalog omits required baseline or
/// image choices. Image/kernel compatibility is validated when building each variant.
pub(crate) fn enabled(schema: &Schema, resolved: &Resolved) -> Result<Vec<Variant>, VariantError> {
    let string = |id: &'static str| match resolved.value(schema, id) {
        Some(Value::Str(value)) => Ok(value.clone()),
        _ => Err(VariantError { option: id }),
    };
    let x86_minimum = string("cpu.x86_64_level")?;
    let arm_minimum = string("cpu.aarch64_level")?;
    let image = string("image.config")?;
    Ok([
        ("x86_64", "v3", "cpu.x86_64_v3"),
        ("x86_64", "v4", "cpu.x86_64_v4"),
        ("aarch64", "armv8.0-a", "cpu.aarch64_v8_0"),
        ("aarch64", "armv8.2-a", "cpu.aarch64_v8_2"),
        ("aarch64", "armv9-a", "cpu.aarch64_v9"),
    ]
    .into_iter()
    .filter(|(_, _, id)| resolved.lookup(schema, id) == Some(Value::Bool(true)))
    .map(|(arch, level, _)| Variant {
        arch,
        level,
        minimum: if arch == "x86_64" {
            x86_minimum.clone()
        } else {
            arm_minimum.clone()
        },
        image: image.clone(),
    })
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_catalog_without_cpu_baselines_returns_a_typed_error() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("features.toml");
        std::fs::write(&catalog, "[[menu]]\nid='example'\ntitle='Example'\n[[option]]\nid='example.enabled'\nmenu='example'\ntype='bool'\ntitle='Enabled'\ndefault=true\n").unwrap();
        let schema = Schema::load(&catalog).unwrap();
        let resolved = crate::model::resolve(&schema, &Default::default());
        let Err(error) = enabled(&schema, &resolved) else {
            panic!("missing CPU choices must be diagnosed");
        };
        assert_eq!(error.option, "cpu.x86_64_level");
    }
}
