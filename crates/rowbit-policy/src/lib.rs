//! The `rowbit.yml` policy file, schema version 0.
//!
//! See `docs/spec/policy.md` for the format and its semantics. In short:
//! rules are keyed by agent, evaluated in order, first match wins, and the
//! result can only be tightened by Rowbit's built-in floors, never loosened.

mod eval;
mod schema;
mod scopes;

pub use eval::{ChangeFacts, Verdict, evaluate, glob};
pub use schema::{
    AgentPolicy, Decision, Match, OneOrMany, PolicyFile, Range, Rule, StatementKind, SumMatch,
};
pub use scopes::{AGENT_USER_PREFIX, DenyRead, MAX_SCOPED_AGENT_ID, Operation, Scopes, agent_user};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("rowbit.yml is not valid YAML for schema v0: {0}")]
    Parse(#[from] serde_norway::Error),
    #[error("rowbit.yml: {0}")]
    Invalid(String),
}

impl PolicyFile {
    /// Parses and validates a policy file. Unknown keys are errors.
    pub fn from_yaml(src: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile = serde_norway::from_str(src)?;
        file.validate()?;
        Ok(file)
    }
}
