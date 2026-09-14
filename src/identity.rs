#[derive(Debug, Clone)]
pub struct IdentityContext {
    pub principal: String,
    pub roles: Vec<String>,
    pub tenant: String,
}

impl IdentityContext {
    pub fn new(principal: String, roles: Vec<String>, tenant: String) -> Self {
        IdentityContext {
            principal,
            roles,
            tenant,
        }
    }

    pub fn anonymous() -> Self {
        IdentityContext {
            principal: String::new(),
            roles: Vec::new(),
            tenant: String::new(),
        }
    }

    pub fn is_authenticated(&self) -> bool {
        !self.principal.is_empty()
    }
}

/// Per-request context supplied by the application.
///
/// Carries the caller identity and an optional TTL that flows to the data
/// plane. Build once per logical request and pass to `CacheManager`
/// operations. Construct with [`CacheContext::new`] or the
/// [`CacheContext::anonymous`] default, then refine with the builder methods.
#[derive(Debug, Clone)]
pub struct CacheContext {
    identity: IdentityContext,
    ttl: Option<std::time::Duration>,
}

impl CacheContext {
    pub fn new(identity: IdentityContext) -> Self {
        CacheContext {
            identity,
            ttl: None,
        }
    }

    pub fn anonymous() -> Self {
        CacheContext {
            identity: IdentityContext::anonymous(),
            ttl: None,
        }
    }

    pub fn with_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    pub fn identity(&self) -> &IdentityContext {
        &self.identity
    }

    pub fn ttl(&self) -> Option<std::time::Duration> {
        self.ttl
    }

    pub fn is_authenticated(&self) -> bool {
        self.identity.is_authenticated()
    }
}

impl Default for CacheContext {
    fn default() -> Self {
        CacheContext::anonymous()
    }
}

impl From<IdentityContext> for CacheContext {
    fn from(identity: IdentityContext) -> Self {
        CacheContext::new(identity)
    }
}
