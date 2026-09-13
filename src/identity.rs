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
