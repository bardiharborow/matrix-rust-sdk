// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Preview of a room, whether we've joined it/left it/been invited to it, or
//! not.
//!
//! This offers a few capabilities for previewing the content of the room as
//! well.

use std::collections::HashSet;

use futures_util::future::join_all;
use matrix_sdk_base::{RawStateEventWithKeys, RoomHeroWithProfile, RoomInfo, RoomState};
use ruma::{
    OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedServerName, RoomId, RoomOrAliasId,
    RoomVersionId, ServerName,
    api::client::{
        membership::{joined_members, joined_rooms},
        state::get_state_events,
    },
    events::room::history_visibility::HistoryVisibility,
    room::{JoinRuleSummary, RoomType},
};
use tokio::try_join;
use tracing::{instrument, warn};

use crate::{Client, Error, Room, room_directory_search::RoomDirectorySearch};

/// The preview of a room, be it invited/joined/left, or not.
#[derive(Debug, Clone)]
pub struct RoomPreview {
    /// The actual room id for this room.
    ///
    /// Remember the room preview can be fetched from a room alias id, so we
    /// might not know ahead of time what the room id is.
    pub room_id: OwnedRoomId,

    /// The canonical alias for the room.
    pub canonical_alias: Option<OwnedRoomAliasId>,

    /// The room's name, if set.
    pub name: Option<String>,

    /// The room's topic, if set.
    pub topic: Option<String>,

    /// The MXC URI to the room's avatar, if set.
    pub avatar_url: Option<OwnedMxcUri>,

    /// The number of joined members.
    pub num_joined_members: u64,

    /// The number of active members, if known (joined + invited).
    pub num_active_members: Option<u64>,

    /// The room type (space, custom) or nothing, if it's a regular room.
    pub room_type: Option<RoomType>,

    /// What's the join rule for this room?
    pub join_rule: Option<JoinRuleSummary>,

    /// The version of the room, if known.
    pub room_version: Option<RoomVersionId>,

    /// Is the room world-readable (i.e. is its history_visibility set to
    /// world_readable)?
    pub is_world_readable: Option<bool>,

    /// Has the current user been invited/joined/left this room?
    ///
    /// Set to `None` if the room is unknown to the user.
    pub state: Option<RoomState>,

    /// The `m.room.direct` state of the room, if known.
    pub is_direct: Option<bool>,

    /// Room heroes.
    pub heroes: Option<Vec<RoomHeroWithProfile>>,
}

/// Whether the current user satisfies the `allow` conditions of a room's join
/// rule.
///
/// See [`RoomPreview::satisfies_join_rule_allow_conditions()`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum JoinRuleAllowConditions {
    /// The current user satisfies at least one of the conditions, and can join
    /// the room without an invite.
    Satisfied,

    /// The current user doesn't satisfy any of the conditions, or the join
    /// rule doesn't allow joining based on conditions.
    NotSatisfied,

    /// It isn't known whether the current user satisfies the conditions.
    Unknown,
}

impl RoomPreview {
    /// Constructs a [`RoomPreview`] from the associated room info.
    ///
    /// Note: not using the room info's state/count of joined members, because
    /// we can do better than that.
    fn from_room_info(
        room_info: RoomInfo,
        is_direct: Option<bool>,
        num_joined_members: u64,
        num_active_members: Option<u64>,
        state: Option<RoomState>,
        computed_display_name: Option<String>,
    ) -> Self {
        RoomPreview {
            room_id: room_info.room_id().to_owned(),
            canonical_alias: room_info.canonical_alias().map(ToOwned::to_owned),
            name: computed_display_name.or_else(|| room_info.name().map(ToOwned::to_owned)),
            topic: room_info.topic().map(ToOwned::to_owned),
            avatar_url: room_info.avatar_url().map(ToOwned::to_owned),
            room_type: room_info.room_type().cloned(),
            join_rule: room_info.join_rule().cloned().map(Into::into),
            room_version: room_info.room_version().cloned(),
            is_world_readable: room_info
                .history_visibility()
                .map(|vis| *vis == HistoryVisibility::WorldReadable),
            num_joined_members,
            num_active_members,
            state,
            is_direct,
            heroes: Some(
                room_info.heroes().iter().cloned().map(RoomHeroWithProfile::from).collect(),
            ),
        }
    }

    /// Create a room preview from a known room.
    ///
    /// Note this shouldn't be used with invited or knocked rooms, since the
    /// local info may be out of date and no longer represent the latest room
    /// state.
    pub(crate) async fn from_known_room(room: &Room) -> Self {
        let is_direct = room.is_direct().await.ok();

        let display_name = room.display_name().await.ok().map(|name| name.to_string());

        Self::from_room_info(
            room.clone_info(),
            is_direct,
            room.joined_members_count(),
            Some(room.active_members_count()),
            Some(room.state()),
            display_name,
        )
    }

    #[instrument(skip(client))]
    pub(crate) async fn from_remote_room(
        client: &Client,
        room_id: OwnedRoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Self> {
        // Use the room summary endpoint, if available, as described in
        // https://github.com/deepbluev7/matrix-doc/blob/room-summaries/proposals/3266-room-summary.md
        match Self::from_room_summary(client, room_id.clone(), room_or_alias_id, via.clone()).await
        {
            Ok(res) => return Ok(res),
            Err(err) => {
                warn!("error when previewing room from the room summary endpoint: {err}");
            }
        }

        // Try room directory search next.
        match Self::from_room_directory_search(client, &room_id, room_or_alias_id, via).await {
            Ok(Some(res)) => return Ok(res),
            Ok(None) => warn!("Room '{room_or_alias_id}' not found in room directory search."),
            Err(err) => {
                warn!("Searching for '{room_or_alias_id}' in room directory search failed: {err}");
            }
        }

        // Try using the room state endpoint, as well as the joined members one.
        match Self::from_state_events(client, &room_id).await {
            Ok(res) => return Ok(res),
            Err(err) => {
                warn!("error when building room preview from state events: {err}");
            }
        }

        // Finally, if everything else fails, try to build the room from
        // information that the client itself might have about it.
        if let Some(room) = client.get_room(&room_id) {
            Ok(Self::from_known_room(&room).await)
        } else {
            Err(Error::InsufficientData)
        }
    }

    /// Whether the current user satisfies at least one of the `allow`
    /// conditions of this room's `restricted` or `knock_restricted` join
    /// rule, i.e. whether they can join the room without an invite and
    /// without knocking.
    ///
    /// Only `m.room_membership` conditions are supported: they're satisfied if
    /// the current user is joined to the given room. This is checked against
    /// the local store first and, if none of the allowed rooms is joined there,
    /// against the homeserver's list of joined rooms for the allowed rooms the
    /// local store doesn't know about, since it may not have synced all the
    /// rooms yet.
    ///
    /// Returns:
    ///
    /// - [`JoinRuleAllowConditions::NotSatisfied`] if the current user is
    ///   banned from the room, if the join rule isn't `restricted` or
    ///   `knock_restricted`, or if the room version doesn't support that join
    ///   rule.
    /// - [`JoinRuleAllowConditions::Unknown`] if the join rule, the room
    ///   version rules or the allowed rooms aren't known. Notably, an empty
    ///   list of allowed rooms is considered unknown, since previews from the
    ///   room directory and some homeservers don't include it, and it doesn't
    ///   include conditions other than `m.room_membership`. This is also
    ///   returned if the request to the homeserver fails.
    pub async fn satisfies_join_rule_allow_conditions(
        &self,
        client: &Client,
    ) -> crate::Result<JoinRuleAllowConditions> {
        if self.state == Some(RoomState::Banned) {
            return Ok(JoinRuleAllowConditions::NotSatisfied);
        }

        let Some(join_rule) = &self.join_rule else {
            return Ok(JoinRuleAllowConditions::Unknown);
        };

        let (is_knock, summary) = match join_rule {
            JoinRuleSummary::Restricted(summary) => (false, summary),
            JoinRuleSummary::KnockRestricted(summary) => (true, summary),
            _ => return Ok(JoinRuleAllowConditions::NotSatisfied),
        };

        if let Some(room_version) = &self.room_version {
            let Some(rules) = room_version.rules() else {
                return Ok(JoinRuleAllowConditions::Unknown);
            };

            let is_supported = if is_knock {
                rules.authorization.knock_restricted_join_rule
            } else {
                rules.authorization.restricted_join_rule
            };

            if !is_supported {
                return Ok(JoinRuleAllowConditions::NotSatisfied);
            }
        }

        if summary.allowed_room_ids.is_empty() {
            return Ok(JoinRuleAllowConditions::Unknown);
        }

        let base_client = client.base_client();
        if summary.allowed_room_ids.iter().any(|room_id| {
            base_client.get_room(room_id).is_some_and(|room| room.state() == RoomState::Joined)
        }) {
            return Ok(JoinRuleAllowConditions::Satisfied);
        }

        // Rooms known locally but not joined can't satisfy the conditions, so
        // only ask the homeserver about the unknown ones.
        let unknown_room_ids: Vec<_> = summary
            .allowed_room_ids
            .iter()
            .filter(|room_id| base_client.get_room(room_id).is_none())
            .collect();

        if unknown_room_ids.is_empty() {
            return Ok(JoinRuleAllowConditions::NotSatisfied);
        }

        let response = match client.send(joined_rooms::v3::Request::new()).await {
            Ok(response) => response,
            Err(err) => {
                warn!("error when fetching the joined rooms: {err}");
                return Ok(JoinRuleAllowConditions::Unknown);
            }
        };

        let joined_room_ids: HashSet<_> = response.joined_rooms.iter().collect();

        Ok(if unknown_room_ids.iter().any(|room_id| joined_room_ids.contains(room_id)) {
            JoinRuleAllowConditions::Satisfied
        } else {
            JoinRuleAllowConditions::NotSatisfied
        })
    }

    /// Get a [`RoomPreview`] by searching in the room directory for the
    /// provided room alias or room id and transforming the [`RoomDescription`]
    /// into a preview.
    pub(crate) async fn from_room_directory_search(
        client: &Client,
        room_id: &RoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Option<Self>> {
        // Get either the room alias or the room id without the leading
        // identifier char
        let search_term = if room_or_alias_id.is_room_alias_id() {
            Some(room_or_alias_id.as_str()[1..].to_owned())
        } else {
            None
        };

        // If we have no alias, filtering using a room id is impossible, so just
        // take the first 100 results and try to find the current room #YOLO
        let batch_size = if search_term.is_some() { 20 } else { 100 };

        if via.is_empty() {
            // Just search in the current homeserver
            search_for_room_preview_in_room_directory(
                client.clone(),
                search_term,
                batch_size,
                None,
                room_id,
            )
            .await
        } else {
            let mut futures = Vec::new();
            // Search for all servers and retrieve the results
            for server in via {
                futures.push(search_for_room_preview_in_room_directory(
                    client.clone(),
                    search_term.clone(),
                    batch_size,
                    Some(server),
                    room_id,
                ));
            }

            let joined_results = join_all(futures).await;

            Ok(joined_results.into_iter().flatten().next().flatten())
        }
    }

    /// Get a [`RoomPreview`] using MSC3266, if available on the remote server.
    ///
    /// Will fail with a 404 if the API is not available.
    ///
    /// This method is exposed for testing purposes; clients should prefer
    /// `Client::get_room_preview` in general over this.
    pub async fn from_room_summary(
        client: &Client,
        room_id: OwnedRoomId,
        room_or_alias_id: &RoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> crate::Result<Self> {
        let own_server_name = client.session_meta().map(|s| s.user_id.server_name());
        let via = ensure_server_names_is_not_empty(own_server_name, via, room_or_alias_id);

        let request = ruma::api::client::room::get_summary::v1::Request::new(
            room_or_alias_id.to_owned(),
            via,
        );

        let response = client.send(request).await?;

        // The server returns a `Left` room state for rooms the user has not
        // joined. Be more precise than that, and set it to `None` if we haven't
        // joined that room. Keep a ban though, since it means the user can't
        // join the room.
        let cached_room = client.get_room(&room_id);
        let state = response.membership.map(|membership| RoomState::from(&membership));
        let state =
            if cached_room.is_none() && state != Some(RoomState::Banned) { None } else { state };

        let num_active_members = cached_room.as_ref().map(|r| r.active_members_count());

        let is_direct = if let Some(cached_room) = &cached_room {
            cached_room.is_direct().await.ok()
        } else {
            None
        };

        let heroes = if let Some(cached_room) = &cached_room {
            Some(cached_room.heroes().await)
        } else {
            None
        };

        let summary = response.summary;

        Ok(RoomPreview {
            room_id,
            canonical_alias: summary.canonical_alias,
            name: summary.name,
            topic: summary.topic,
            avatar_url: summary.avatar_url,
            num_joined_members: summary.num_joined_members.into(),
            num_active_members,
            room_type: summary.room_type,
            join_rule: Some(summary.join_rule),
            room_version: summary.room_version,
            is_world_readable: Some(summary.world_readable),
            state,
            is_direct,
            heroes,
        })
    }

    /// Get a [`RoomPreview`] using the room state endpoint.
    ///
    /// This is always available on a remote server, but will only work if one
    /// of these two conditions is true:
    ///
    /// - the user has joined the room at some point (i.e. they're still joined
    ///   or they've joined it and left it later).
    /// - the room has an history visibility set to world-readable.
    ///
    /// This method is exposed for testing purposes; clients should prefer
    /// `Client::get_room_preview` in general over this.
    pub async fn from_state_events(client: &Client, room_id: &RoomId) -> crate::Result<Self> {
        let state_request = get_state_events::v3::Request::new(room_id.to_owned());
        let joined_members_request = joined_members::v3::Request::new(room_id.to_owned());

        let (state, joined_members) =
            try_join!(async { client.send(state_request).await }, async {
                client.send(joined_members_request).await
            })?;

        // Converting from usize to u64 will always work, up to 64-bits devices;
        // otherwise, assume LOTS of members.
        let num_joined_members = joined_members.joined.len().try_into().unwrap_or(u64::MAX);

        let mut room_info = RoomInfo::new(room_id, RoomState::Joined);

        for ev in state.room_state {
            if let Some(mut raw_event) = RawStateEventWithKeys::try_from_raw_state_event(ev.cast())
            {
                room_info.handle_state_event(&mut raw_event);
            }
        }

        let room = client.get_room(room_id);
        let state = room.as_ref().map(|room| room.state());
        let num_active_members = room.as_ref().map(|r| r.active_members_count());
        let is_direct = if let Some(room) = room { room.is_direct().await.ok() } else { None };

        Ok(Self::from_room_info(
            room_info,
            is_direct,
            num_joined_members,
            num_active_members,
            state,
            None,
        ))
    }
}

async fn search_for_room_preview_in_room_directory(
    client: Client,
    filter: Option<String>,
    batch_size: u32,
    server: Option<OwnedServerName>,
    expected_room_id: &RoomId,
) -> crate::Result<Option<RoomPreview>> {
    let mut directory_search = RoomDirectorySearch::new(client);
    directory_search.search(filter, batch_size, server).await?;

    let (results, _) = directory_search.results();

    for room_description in results {
        // Iterate until we find a room description with a matching room id
        if room_description.room_id != expected_room_id {
            continue;
        }
        return Ok(Some(RoomPreview {
            room_id: room_description.room_id,
            canonical_alias: room_description.alias,
            name: room_description.name,
            topic: room_description.topic,
            avatar_url: room_description.avatar_url,
            num_joined_members: room_description.joined_members,
            num_active_members: None,
            // Assume it's a room
            room_type: None,
            join_rule: Some(room_description.join_rule.into()),
            room_version: None,
            is_world_readable: Some(room_description.is_world_readable),
            state: None,
            is_direct: None,
            heroes: None,
        }));
    }

    Ok(None)
}

// Make sure the server name of the room id/alias is included in the list of
// server names to send if no server names are provided
fn ensure_server_names_is_not_empty(
    own_server_name: Option<&ServerName>,
    server_names: Vec<OwnedServerName>,
    room_or_alias_id: &RoomOrAliasId,
) -> Vec<OwnedServerName> {
    let mut server_names = server_names;

    if let Some((own_server, alias_server)) = own_server_name.zip(room_or_alias_id.server_name())
        && server_names.is_empty()
        && own_server != alias_server
    {
        server_names.push(alias_server.to_owned());
    }

    server_names
}

#[cfg(test)]
mod tests {
    use ruma::{RoomOrAliasId, ServerName, room_alias_id, room_id, server_name};

    use crate::room_preview::ensure_server_names_is_not_empty;

    #[test]
    fn test_ensure_server_names_is_not_empty_when_no_own_server_name_is_provided() {
        let own_server_name: Option<&ServerName> = None;
        let room_or_alias_id: &RoomOrAliasId = room_id!("!test:localhost").into();

        let server_names =
            ensure_server_names_is_not_empty(own_server_name, Vec::new(), room_or_alias_id);

        // There was no own server name to check against, so no additional
        // server name was added
        assert!(server_names.is_empty());
    }

    #[test]
    fn test_ensure_server_names_is_not_empty_when_room_alias_or_id_has_no_server_name() {
        let own_server_name: Option<&ServerName> = Some(server_name!("localhost"));
        let room_or_alias_id: &RoomOrAliasId = room_id!("!test").into();

        let server_names =
            ensure_server_names_is_not_empty(own_server_name, Vec::new(), room_or_alias_id);

        // The room id has no server name, so nothing could be added
        assert!(server_names.is_empty());
    }

    #[test]
    fn test_ensure_server_names_is_not_empty_with_same_server_name() {
        let own_server_name: Option<&ServerName> = Some(server_name!("localhost"));
        let room_or_alias_id: &RoomOrAliasId = room_id!("!test:localhost").into();

        let server_names =
            ensure_server_names_is_not_empty(own_server_name, Vec::new(), room_or_alias_id);

        // The room id's server name was the same as our own server name, so
        // there's no need to add it
        assert!(server_names.is_empty());
    }

    #[test]
    fn test_ensure_server_names_is_not_empty_with_different_room_id_server_name() {
        let own_server_name: Option<&ServerName> = Some(server_name!("localhost"));
        let room_or_alias_id: &RoomOrAliasId = room_id!("!test:matrix.org").into();

        let server_names =
            ensure_server_names_is_not_empty(own_server_name, Vec::new(), room_or_alias_id);

        // The server name in the room id was added
        assert_eq!(server_names, &["matrix.org"]);
    }

    #[test]
    fn test_ensure_server_names_is_not_empty_with_different_room_alias_server_name() {
        let own_server_name: Option<&ServerName> = Some(server_name!("localhost"));
        let room_or_alias_id: &RoomOrAliasId = room_alias_id!("#test:matrix.org").into();

        let server_names =
            ensure_server_names_is_not_empty(own_server_name, Vec::new(), room_or_alias_id);

        // The server name in the room alias was added
        assert_eq!(server_names, &["matrix.org"]);
    }
}
