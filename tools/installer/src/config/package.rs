//! Package selection forms accepted by installer TOML files.

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
/// A package selection with an optional build rule or source override.
pub enum PackageConfig {
    /// Install the default package version.
    Empty,
    /// Select a cookbook build rule, such as `source`.
    Build(String),

    // TODO: Sum type
    /// Override package version or source.
    Spec {
        /// Optional package version constraint.
        version: Option<String>,
        /// Optional source Git repository.
        git: Option<String>,
        /// Optional source directory.
        path: Option<String>,
    },
}

impl Default for PackageConfig {
    fn default() -> Self {
        Self::Empty
    }
}
