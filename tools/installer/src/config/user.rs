//! User and group account declarations for installation.

use std::fmt::Display;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// A user account to create in the installed image.
pub struct UserConfig {
    /// Optional account password; an empty value requests no password.
    pub password: Option<String>,
    /// Explicit user ID, or automatic allocation when absent.
    pub uid: Option<u32>,
    /// Explicit primary group ID, or automatic allocation when absent.
    pub gid: Option<u32>,
    /// Human-readable account name.
    pub name: Option<String>,
    /// Home directory in the installed image.
    pub home: Option<String>,
    /// Login shell path in the installed image.
    pub shell: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// A group account to create in the installed image.
pub struct GroupConfig {
    /// Explicit primary group ID, or automatic allocation when absent.
    pub gid: Option<u32>,
    // FIXME move this to the UserConfig struct as extra_groups
    /// Login names included in this group.
    pub members: Vec<String>,
}

impl Display for UserConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(uid) = &self.uid {
            write!(f, " uid={}", uid)?;
        }
        if let Some(gid) = &self.gid {
            write!(f, " gid={}", gid)?;
        }
        if let Some(name) = &self.name {
            write!(f, " name={}", name)?;
        }
        if let Some(home) = &self.home {
            write!(f, " home={}", home)?;
        }
        if let Some(shell) = &self.shell {
            write!(f, " shell={}", shell)?;
        }
        if self.password.as_ref().is_some_and(|s| !s.is_empty()) {
            write!(f, " password=yes")?;
        }

        Ok(())
    }
}
