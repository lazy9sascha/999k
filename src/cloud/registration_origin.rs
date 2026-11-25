use url::Url;

pub(crate) fn https_origin(input: &str) -> Result<Url, String> {
    if !input.strip_prefix("https://").is_some_and(|rest| {
        !rest
            .strip_suffix('/')
            .unwrap_or(rest)
            .contains(['/', '\\', '@', '?', '#'])
    }) || input
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err("registration origin must be an HTTPS origin".into());
    }
    Url::parse(input)
        .ok()
        .filter(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| "registration origin must be an HTTPS origin".into())
}
