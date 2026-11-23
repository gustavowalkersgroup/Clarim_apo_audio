//! Which errors may reach the dashboard as they are.
//!
//! The service's work (registry, installer, service control) fails in many ways whose text is English and
//! technical; the dashboard shows PT-BR sentences. An error made with `user_err` / `user_bail!` carries a
//! PT-BR sentence meant for the user; `user_text` finds it anywhere in an error chain (a `PlanError` counts
//! too) and otherwise returns the caller's fallback. The technical chain goes to the log.

use std::fmt;

use crate::apo_plan::PlanError;

#[derive(Debug)]
pub(crate) struct UserError(pub String);

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UserError {}

pub(crate) fn user_err(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UserError(message.into()))
}

/// Returns early with a user-facing (PT-BR) error.
macro_rules! user_bail {
    ($($t:tt)*) => {
        return Err($crate::user_error::user_err(format!($($t)*)))
    };
}

/// The PT-BR sentence inside `e`, or `fallback`.
pub(crate) fn user_text(e: &anyhow::Error, fallback: &str) -> String {
    for cause in e.chain() {
        if let Some(user) = cause.downcast_ref::<UserError>() {
            return user.0.clone();
        }
        if let Some(plan) = cause.downcast_ref::<PlanError>() {
            return plan.to_string();
        }
    }
    fallback.to_string()
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn a_user_sentence_survives_technical_context_and_anything_else_gets_the_fallback() {
        let inner: anyhow::Result<()> = Err(user_err("Sem internet."));
        let wrapped = inner.context("cannot run curl").unwrap_err();
        assert_eq!(user_text(&wrapped, "falha"), "Sem internet.");
        let plan = anyhow::Error::new(PlanError::NoRecord).context("planning");
        assert!(user_text(&plan, "falha").contains("desinstalador"));
        assert_eq!(user_text(&anyhow::anyhow!("registry: open failed (Win32 error 5)"), "falha"), "falha");
        fn bails() -> anyhow::Result<()> {
            user_bail!("Valor {}.", 3);
        }
        assert_eq!(bails().unwrap_err().to_string(), "Valor 3.");
    }
}
