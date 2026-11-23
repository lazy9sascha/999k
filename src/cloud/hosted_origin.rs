//! An opaque, host-local origin selected by a new installation.

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UnixOrigin(String);

impl UnixOrigin {
    pub(crate) fn parse(nonce: &str) -> Option<Self> {
        (nonce.len() == 32
            && nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| Self(nonce.into()))
    }

    #[allow(dead_code)] // The operator uses this field; the local fixture does not.
    pub(crate) fn nonce(&self) -> &str {
        &self.0
    }

    pub(crate) fn directory_name(&self) -> String {
        format!("pmq-{}", self.0)
    }

    pub(crate) fn socket_path(&self) -> String {
        format!("/private/tmp/{}/hub.sock", self.directory_name())
    }
}
