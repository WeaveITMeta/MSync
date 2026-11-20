//! Slack as a [`SyncPlatform`]: a job title is membership in a user group (contract 12.2).
//!
//! The managed user groups are the `slack_usergroup` ids in `roles.toml`; no other user group
//! is read or written. A user group carries no permissions, so every mapped group is safe to
//! manage. Slack edits a group by replacing its whole member list, so adding or removing one
//! member reads the list and writes it back. Slack refuses an empty user group, so the last
//! member of a group is never removed; that change is reported as refused.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;

use super::api::SlackApi;
use crate::chat::sync::{PlatformError, RoleSafety, SyncPlatform};
use crate::chat::ChatPlatform;
use crate::cra::loader::CraHandle;

pub struct SlackPlatform {
    pub api: Arc<dyn SlackApi>,
    pub cra: CraHandle,
}

impl SlackPlatform {
    pub fn new(api: Arc<dyn SlackApi>, cra: CraHandle) -> Self {
        SlackPlatform { api, cra }
    }

    fn alert_channel(&self) -> Option<String> {
        self.cra
            .active()?
            .snapshot
            .org
            .slack
            .as_ref()?
            .alert_channel_id
            .clone()
    }
}

#[async_trait]
impl SyncPlatform for SlackPlatform {
    fn platform(&self) -> ChatPlatform {
        ChatPlatform::Slack
    }

    /// A bot token, and a CRA with a `[slack]` section.
    fn configured(&self) -> bool {
        self.api.configured()
            && self
                .cra
                .active()
                .is_some_and(|a| a.snapshot.org.slack.is_some())
    }

    async fn role_safety(
        &self,
        roles: &[String],
    ) -> Result<BTreeMap<String, RoleSafety>, PlatformError> {
        Ok(roles
            .iter()
            .map(|r| (r.clone(), RoleSafety::Safe))
            .collect())
    }

    async fn holders(
        &self,
        roles: &[String],
    ) -> Result<BTreeMap<String, BTreeSet<String>>, PlatformError> {
        let mut out = BTreeMap::new();
        for r in roles {
            out.insert(
                r.clone(),
                self.api.usergroup_users(r).await?.into_iter().collect(),
            );
        }
        Ok(out)
    }

    async fn add_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        let mut users = self.api.usergroup_users(role).await?;
        if users.iter().any(|u| u == user) {
            return Ok(());
        }
        users.push(user.to_string());
        users.sort();
        self.api.set_usergroup_users(role, &users).await
    }

    async fn remove_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        let users = self.api.usergroup_users(role).await?;
        if !users.iter().any(|u| u == user) {
            return Ok(());
        }
        let rest: Vec<String> = users.into_iter().filter(|u| u != user).collect();
        if rest.is_empty() {
            return Err(PlatformError::Refused(
                "Slack does not allow an empty user group; disable the group in Slack or add a member in the CRA".into(),
            ));
        }
        self.api.set_usergroup_users(role, &rest).await
    }

    async fn alert(&self, text: &str) -> Result<(), PlatformError> {
        let channel = self.alert_channel().ok_or_else(|| {
            PlatformError::Refused("no [slack] alert_channel_id in org.toml".into())
        })?;
        self.api
            .post_message(&channel, &super::to_mrkdwn(text))
            .await
    }
}
