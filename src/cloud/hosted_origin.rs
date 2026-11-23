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

    pub(crate) fn nonce(&self) -> &str {
        &self.0
    }

    pub(crate) fn socket_path(&self) -> String {
        format!("/private/tmp/pmq-{}/hub.sock", self.0)
    }
}
