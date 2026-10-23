//! [`Authorizer`]: the single
//! point where a core operation asks whether its actor may run it.
//!
//! Every operation of a core service that acts for a user calls
//! [`Authorizer::check`] exactly once with its [`Action`] and the
//! [`Resource`] it is about (MOD-04); a list allowed to a user who is not
//! the admin is then narrowed item by item ([`Authorizer::visible`],
//! TEN-02). The `authorize` hook answers first:
//! `deny` refuses with its reason (also for an admin), `allow` admits
//! without the admin check, except for the installation-wide `admin.*`
//! operations ([`Action::is_admin_only`]), which a module can narrow but
//! never open to anyone but the admin. On `continue` the core decides,
//! and without a module that narrows it (`tenants`) only the single admin
//! is allowed (MOD-05).

use crate::domain::User;
use crate::error::Error;
use crate::ports::{Action, HookResult, Hooks as _, Ports, Resource};

/// See the [module documentation](self).
#[derive(Debug)]
pub struct Authorizer<'a, P> {
    ports: &'a P,
}

impl<P> Clone for Authorizer<'_, P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P> Copy for Authorizer<'_, P> {}

impl<'a, P: Ports> Authorizer<'a, P> {
    /// The authorizer over `ports` (it asks their [`Hooks`](crate::ports::Hooks)).
    #[must_use]
    pub const fn new(ports: &'a P) -> Self {
        Self { ports }
    }

    /// `Ok` when `actor` may do `action` on `resource`: the hook allowed
    /// it (an admin-only action also needs `actor` to be the admin), or it
    /// had no opinion and `actor` is an admin.
    ///
    /// # Errors
    ///
    /// [`Error::Forbidden`] with the hook's reason when it denies, or
    /// naming the actor and the action when it has no opinion and `actor`
    /// is not an admin.
    pub async fn check(
        &self,
        actor: &User,
        action: Action,
        resource: &Resource,
    ) -> Result<(), Error> {
        match self.ports.hooks().authorize(actor, action, resource).await {
            HookResult::Deny(reason) => Err(Error::forbidden(reason)),
            HookResult::Allow if !action.is_admin_only() || actor.is_admin() => Ok(()),
            HookResult::Continue if actor.is_admin() => Ok(()),
            HookResult::Allow | HookResult::Continue => Err(Error::forbidden(format!(
                "{} may not {action}",
                actor.login
            ))),
        }
    }

    /// Of `items`, those whose resource `actor` may see for the list
    /// `action`, once [`Authorizer::check`] admitted the list as a whole
    /// (about [`Resource::Admin`]).
    ///
    /// The admin sees every item and nothing more is asked, so the list
    /// asks `authorize` exactly once (MOD-04). Anyone else got the list
    /// from a module's `allow` (`tenants`: every user lists), so the hook
    /// is asked once per item about that item's resource with the same
    /// action, and only an `allow` keeps it (TEN-02: a member sees the
    /// projects of their tenants).
    pub async fn visible<T>(
        &self,
        actor: &User,
        action: Action,
        items: Vec<T>,
        resource: impl Fn(&T) -> Resource,
    ) -> Vec<T> {
        if actor.is_admin() {
            return items;
        }
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            let answer = self
                .ports
                .hooks()
                .authorize(actor, action, &resource(&item))
                .await;
            if answer == HookResult::Allow {
                kept.push(item);
            }
        }
        kept
    }
}

#[cfg(test)]
mod tests;
