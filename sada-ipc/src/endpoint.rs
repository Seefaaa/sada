//! Where a channel is listened for or connected to.

use std::{
    fmt::{self, Display},
    path::{Path, PathBuf},
};

use serde::Deserialize;

/// Address of an IPC channel.
///
/// One string serves both platforms. On unix it is the path of the socket file. On windows it names a pipe under
/// `\\.\pipe\`: a name that already starts with `\\` is taken as it stands, and anything else contributes its last
/// path segment, so `/tmp/sada.sock` and `\\.\pipe\sada.sock` are the same endpoint written for two hosts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(from = "String")]
pub struct Endpoint(String);

impl Endpoint {
    /// Name an endpoint, putting it where this platform keeps them.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self { Self(normalize(name.into())) }

    /// The endpoint as the platform spells it.
    #[must_use]
    pub fn as_str(&self) -> &str { &self.0 }

    /// The path of the socket file.
    #[cfg(unix)]
    pub(crate) fn path(&self) -> &Path { Path::new(&self.0) }
}

/// Put `name` where this platform keeps its endpoints.
#[cfg(unix)]
fn normalize(name: String) -> String { name }

/// Put `name` under `\\.\pipe\`, unless it is already a full pipe name.
#[cfg(windows)]
fn normalize(name: String) -> String {
    if name.starts_with(r"\\") {
        return name;
    }

    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(&name);

    format!(r"\\.\pipe\{leaf}")
}

impl Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

impl From<String> for Endpoint {
    fn from(value: String) -> Self { Self::new(value) }
}

impl From<&str> for Endpoint {
    fn from(value: &str) -> Self { Self::new(value) }
}

impl From<PathBuf> for Endpoint {
    fn from(value: PathBuf) -> Self { Self::new(value.to_string_lossy().into_owned()) }
}

impl From<&Path> for Endpoint {
    fn from(value: &Path) -> Self { Self::new(value.to_string_lossy().into_owned()) }
}

#[cfg(test)]
mod tests {
    use super::Endpoint;

    #[test]
    #[cfg(unix)]
    fn a_unix_endpoint_is_the_path_it_was_given() {
        let endpoint = Endpoint::new("/tmp/sada.sock");

        assert_eq!(endpoint.as_str(), "/tmp/sada.sock");
        assert_eq!(endpoint.path(), std::path::Path::new("/tmp/sada.sock"));
    }

    #[test]
    #[cfg(windows)]
    fn a_path_becomes_a_pipe_name() {
        assert_eq!(Endpoint::new("/tmp/sada.sock").as_str(), r"\\.\pipe\sada.sock");
        assert_eq!(Endpoint::new("sada").as_str(), r"\\.\pipe\sada");
    }

    #[test]
    #[cfg(windows)]
    fn a_full_pipe_name_is_left_alone() {
        assert_eq!(Endpoint::new(r"\\.\pipe\sada").as_str(), r"\\.\pipe\sada");
    }
}
