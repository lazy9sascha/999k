// Synthetic service backend for receiver tests; production uses the HTTP client.
pub struct LocalTransport {
    pub db: std::path::PathBuf,
    pub registration: std::path::PathBuf,
}
impl radio999k::receiver_client::Transport for LocalTransport {
    fn call(
        &self,
        request: radio999k::receiver_api::Request,
    ) -> std::result::Result<serde_json::Value, &'static str> {
        radio999k::receiver_api::execute(
            &self.db,
            &self.registration,
            "codex-local",
            request,
            || Ok(()),
        )
        .map_err(|error| match error {
            radio999k::Error::Invalid(code) => code,
            _ => "receiver_api_failed",
        })
    }
}
pub fn open(
    db: &std::path::Path,
    binding: radio999k::adapters::codex::receiver::Binding,
) -> std::result::Result<radio999k::adapters::codex::receiver::Receiver, &'static str> {
    let transport = LocalTransport {
        db: db.into(),
        registration: binding.registration.clone(),
    };
    radio999k::adapters::codex::receiver::Receiver::open(
        &db.with_extension("receiver-state"),
        binding,
        Box::new(transport),
    )
}
