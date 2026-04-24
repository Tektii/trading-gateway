use secrecy::SecretBox;

pub struct AlpacaCredentials {
    pub api_key: SecretBox<String>,
    pub api_secret: SecretBox<String>,
    pub feed: Option<String>,
    pub base_url: Option<String>,
    pub data_url: Option<String>,
}

impl AlpacaCredentials {
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: SecretBox::new(Box::new(api_key.into())),
            api_secret: SecretBox::new(Box::new(api_secret.into())),
            feed: None,
            base_url: None,
            data_url: None,
        }
    }

    #[must_use]
    pub fn with_feed(mut self, feed: impl Into<String>) -> Self {
        self.feed = Some(feed.into());
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    #[must_use]
    pub fn with_data_url(mut self, url: impl Into<String>) -> Self {
        self.data_url = Some(url.into());
        self
    }
}

impl std::fmt::Debug for AlpacaCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlpacaCredentials")
            .field("api_key", &"[REDACTED]")
            .field("api_secret", &"[REDACTED]")
            .field("feed", &self.feed)
            .field("base_url", &self.base_url)
            .field("data_url", &self.data_url)
            .finish()
    }
}
