use assert_matches::assert_matches;
use js_int::uint;
use matrix_sdk::{
    room_preview::{JoinRuleAllowConditions, RoomPreview},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::{RequestedRequiredStates, RoomState};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder, async_test,
};
use ruma::{
    OwnedRoomId, RoomVersionId,
    api::client::sync::sync_events::v5::{self as sliding_sync_http, response::Hero},
    assign,
    events::room::{
        join_rules::{AllowRule, JoinRule, Restricted},
        member::MembershipState,
    },
    owned_room_id, owned_user_id,
    room::{JoinRuleSummary, RestrictedSummary},
    room_id,
};
use serde_json::json;

#[async_test]
async fn test_room_preview_leave_invited() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    server
        .mock_room_summary()
        .ok_with_membership(room_id, MembershipState::Invite)
        .mock_once()
        .mount()
        .await;
    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Invited);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_invite_leave_room_summary_msc3266_disabled() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    // A preview should be built from the sync data above
    let preview = client
        .get_room_preview(room_id.into(), Vec::new())
        .await
        .expect("Room preview should be retrieved");

    assert_eq!(preview.room_id, room_id);
    assert_matches!(preview.state.unwrap(), RoomState::Invited);

    server.mock_room_leave().ok(room_id).expect(1).mount().await;

    room.leave().await.unwrap();

    assert_matches!(room.state(), RoomState::Left);
    assert_matches!(
        client.get_room_preview(room_id.into(), Vec::new()).await.unwrap().state.unwrap(),
        RoomState::Left
    );
}

#[async_test]
async fn test_room_preview_leave_knocked() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, KnockedRoomBuilder::new(room_id)).await;

    server
        .mock_room_summary()
        .ok_with_membership(room_id, MembershipState::Knock)
        .mock_once()
        .mount()
        .await;
    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Knocked);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_joined() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id)).await;

    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Joined);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_unknown_room_fails() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    server.mock_room_summary().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert!(room_preview.state.is_none());

    assert!(client.get_room(room_id).is_none());
}

#[async_test]
async fn test_room_preview_computes_name_if_room_is_known() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    // Given a room with no name but a hero
    let room = assign!(sliding_sync_http::response::Room::new(), {
        name: None,
        heroes: Some(vec![assign!(Hero::new(owned_user_id!("@alice:matrix.org")), {
            name: Some("Alice".to_owned()),
            avatar: None,
        })]),
        joined_count: Some(uint!(1)),
        invited_count: Some(uint!(1)),
    });
    let mut response = sliding_sync_http::Response::new("0".to_owned());
    response.rooms.insert(room_id.to_owned(), room);

    client
        .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
        .await
        .expect("Failed to process sync");

    // When we get its preview
    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();

    // Its name is computed from its heroes
    assert_eq!(room_preview.name.unwrap(), "Alice");
}

fn restricted_room_preview(
    join_rule: Option<JoinRuleSummary>,
    room_version: Option<RoomVersionId>,
    state: Option<RoomState>,
) -> RoomPreview {
    RoomPreview {
        room_id: owned_room_id!("!restricted:localhost"),
        canonical_alias: None,
        name: None,
        topic: None,
        avatar_url: None,
        num_joined_members: 0,
        num_active_members: None,
        room_type: None,
        join_rule,
        room_version,
        is_world_readable: None,
        state,
        is_direct: None,
        heroes: None,
    }
}

fn restricted(allowed_room_ids: Vec<OwnedRoomId>) -> Option<JoinRuleSummary> {
    Some(JoinRuleSummary::Restricted(RestrictedSummary::new(allowed_room_ids)))
}

fn knock_restricted(allowed_room_ids: Vec<OwnedRoomId>) -> Option<JoinRuleSummary> {
    Some(JoinRuleSummary::KnockRestricted(RestrictedSummary::new(allowed_room_ids)))
}

#[async_test]
async fn test_satisfies_join_rule_allow_conditions_with_local_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let joined_room_id = owned_room_id!("!joined:localhost");
    let invited_room_id = owned_room_id!("!invited:localhost");

    server.sync_joined_room(&client, &joined_room_id).await;
    server.sync_room(&client, InvitedRoomBuilder::new(&invited_room_id)).await;

    // Being joined to one of the allowed rooms according to the local store
    // satisfies the conditions without asking the homeserver, for both
    // `restricted` and `knock_restricted`. No `/joined_rooms` endpoint is
    // mocked, so a request would fail.
    for join_rule in [
        restricted(vec![joined_room_id.clone()]),
        knock_restricted(vec![invited_room_id.clone(), joined_room_id.clone()]),
    ] {
        let preview = restricted_room_preview(join_rule, Some(RoomVersionId::V10), None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::Satisfied
        );
    }
}

#[async_test]
async fn test_satisfies_join_rule_allow_conditions_with_remote_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let remote_joined_room_id = owned_room_id!("!remote_joined:localhost");
    let invited_room_id = owned_room_id!("!invited:localhost");
    let left_room_id = owned_room_id!("!left:localhost");
    let unknown_room_id = owned_room_id!("!unknown:localhost");

    server.sync_room(&client, InvitedRoomBuilder::new(&invited_room_id)).await;
    server.sync_room(&client, LeftRoomBuilder::new(&left_room_id)).await;
    server.mock_joined_rooms().ok(vec![remote_joined_room_id.clone()]).mount().await;

    // A room that isn't joined according to the local store, but is according
    // to the homeserver, satisfies the conditions.
    for join_rule in [
        restricted(vec![remote_joined_room_id.clone()]),
        knock_restricted(vec![unknown_room_id.clone(), remote_joined_room_id.clone()]),
    ] {
        let preview = restricted_room_preview(join_rule, None, None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::Satisfied
        );
    }

    // Any other membership, or an unknown room, doesn't.
    for room_id in [invited_room_id, left_room_id, unknown_room_id] {
        let preview = restricted_room_preview(restricted(vec![room_id]), None, None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::NotSatisfied
        );
    }

    // Being banned from the room means the user can't join it, whatever the
    // conditions.
    let preview = restricted_room_preview(
        restricted(vec![remote_joined_room_id.clone()]),
        None,
        Some(RoomState::Banned),
    );
    assert_eq!(
        preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
        JoinRuleAllowConditions::NotSatisfied
    );

    // Room versions that don't support the join rule never satisfy the
    // conditions.
    for (join_rule, room_version) in [
        (restricted(vec![remote_joined_room_id.clone()]), RoomVersionId::V7),
        (knock_restricted(vec![remote_joined_room_id.clone()]), RoomVersionId::V9),
    ] {
        let preview = restricted_room_preview(join_rule, Some(room_version), None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::NotSatisfied
        );
    }

    // Other join rules don't have allow conditions.
    for join_rule in [
        JoinRuleSummary::Invite,
        JoinRuleSummary::Knock,
        JoinRuleSummary::Private,
        JoinRuleSummary::Public,
    ] {
        let preview = restricted_room_preview(Some(join_rule), None, None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::NotSatisfied
        );
    }
}

#[async_test]
async fn test_satisfies_join_rule_allow_conditions_unknown() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let joined_room_id = owned_room_id!("!joined:localhost");
    server.sync_joined_room(&client, &joined_room_id).await;

    // Only custom conditions, which are dropped when converting to a
    // `JoinRuleSummary`.
    let custom_allow_rule: AllowRule = serde_json::from_value(json!({
        "type": "org.example.custom",
        "room_id": joined_room_id,
    }))
    .unwrap();
    let custom_join_rule = JoinRule::Restricted(Restricted::new(vec![custom_allow_rule])).into();

    let custom_room_version = RoomVersionId::try_from("org.example.custom").unwrap();

    for (join_rule, room_version) in [
        // The join rule is unknown.
        (None, None),
        // The allowed rooms are unknown, like in a room directory preview.
        (Some(JoinRuleSummary::Restricted(Default::default())), None),
        (knock_restricted(vec![]), Some(RoomVersionId::V10)),
        (Some(custom_join_rule), None),
        // The room version rules are unknown.
        (restricted(vec![joined_room_id.clone()]), Some(custom_room_version)),
    ] {
        let preview = restricted_room_preview(join_rule, room_version, None);
        assert_eq!(
            preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
            JoinRuleAllowConditions::Unknown
        );
    }
}

#[async_test]
async fn test_satisfies_join_rule_allow_conditions_without_remote_request() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let invited_room_id = owned_room_id!("!invited:localhost");
    let left_room_id = owned_room_id!("!left:localhost");

    server.sync_room(&client, InvitedRoomBuilder::new(&invited_room_id)).await;
    server.sync_room(&client, LeftRoomBuilder::new(&left_room_id)).await;
    server.mock_joined_rooms().ok(vec![invited_room_id.clone()]).never().mount().await;

    // All the allowed rooms are known locally and not joined, so the homeserver
    // isn't asked.
    let preview =
        restricted_room_preview(restricted(vec![invited_room_id, left_room_id]), None, None);
    assert_eq!(
        preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
        JoinRuleAllowConditions::NotSatisfied
    );
}

#[async_test]
async fn test_satisfies_join_rule_allow_conditions_remote_request_fails() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_joined_rooms().error500().mount().await;

    let preview =
        restricted_room_preview(restricted(vec![owned_room_id!("!unknown:localhost")]), None, None);
    assert_eq!(
        preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
        JoinRuleAllowConditions::Unknown
    );
}

#[async_test]
async fn test_room_preview_keeps_ban_for_unknown_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    server.mock_room_summary().ok_with_membership(room_id, MembershipState::Ban).mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state, Some(RoomState::Banned));
    assert!(client.get_room(room_id).is_none());

    assert_eq!(
        room_preview.satisfies_join_rule_allow_conditions(&client).await.unwrap(),
        JoinRuleAllowConditions::NotSatisfied
    );
}
