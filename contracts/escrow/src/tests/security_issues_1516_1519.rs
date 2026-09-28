//! Regression tests for security issues #1516, #1517, #1518, and #1519.
//!
//! #1516 — losing player can block oracle settlement forever via pause_match
//! #1517 — dispute_and_rollback_match lets a player refund their stake after losing
//! #1518 — heartbeat_match lets a player postpone admin_resolve_stalled_match indefinitely
//! #1519 — update_player_stats records a win for both players

use super::*;
use soroban_sdk::testutils::Ledger as _;

// ── helpers ──────────────────────────────────────────────────────────────────

fn advance_time(env: &Env, seconds: u64) {
    let ts = env.ledger().timestamp();
    env.ledger().set_timestamp(ts.saturating_add(seconds));
}

fn advance_ledgers(env: &Env, ledgers: u32) {
    env.ledger().with_mut(|l| {
        l.sequence_number = l.sequence_number.saturating_add(ledgers);
    });
}

/// Bring a match to `Active` state.
fn make_active_match(
    client: &EscrowContractClient,
    env: &Env,
    p1: &Address,
    p2: &Address,
    token: &Address,
    game_id: &str,
) -> u64 {
    let id = client.create_match(
        p1,
        p2,
        &100,
        token,
        &String::from_str(env, game_id),
        &Platform::Lichess,
    );
    client.deposit(&id, p1);
    client.deposit(&id, p2);
    id
}

// ── #1516: pause-to-block-settlement attack ───────────────────────────────────

/// A paused match can still be settled by the oracle (#1516 fix).
/// Before the fix, `settle_result` would return `InvalidState` on a Paused match.
#[test]
fn test_1516_oracle_can_settle_paused_match() {
    let (env, contract_id, oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "aa11bb22");

    // Losing player pauses to try to block settlement.
    client.pause_match(&id, &player1);
    assert_eq!(client.get_match(&id).state, MatchState::Paused);

    // Oracle submits result — must succeed on a Paused match.
    client.submit_result(&id, &Winner::Player2, &oracle);

    let m = client.get_match(&id);
    assert_eq!(m.state, MatchState::Completed);
    assert_eq!(m.winner, Winner::Player2);
}

/// Once the total pause budget (MAX_PAUSE_DURATION_LEDGERS) is exhausted, a
/// player cannot re-pause to keep the match in Paused indefinitely (#1516).
#[test]
fn test_1516_pause_budget_exhausted_rejects_new_pause() {
    let (env, contract_id, _oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "cc33dd44");

    // Pause and immediately advance ledgers past the full budget.
    client.pause_match(&id, &player1);
    // Simulate accumulating the maximum pause ledgers by advancing sequence.
    advance_ledgers(&env, MAX_PAUSE_DURATION_LEDGERS);
    // Resume so the match is Active again.
    client.resume_match(&id, &player2);

    // The pause budget was consumed during resume via total_pause_duration.
    // Attempting to pause again should be rejected.
    let result = client.try_pause_match(&id, &player1);
    assert_eq!(
        result,
        Err(Ok(Error::InvalidPauseState)),
        "pause must be rejected once the total pause budget is exhausted"
    );
}

/// Admin can resolve a long-paused match (#1516: admin recovery path for Paused).
#[test]
fn test_1516_admin_can_resolve_paused_match_after_stall_window() {
    let (env, contract_id, _oracle, player1, player2, token, admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    env.ledger().set_timestamp(1_000);
    let id = make_active_match(&client, &env, &player1, &player2, &token, "ee55ff66");

    // Player1 pauses to block settlement.
    client.pause_match(&id, &player1);
    assert_eq!(client.get_match(&id).state, MatchState::Paused);

    // Advance time well past the 7-day admin stall window.
    advance_time(&env, ADMIN_STALL_WINDOW_SECONDS + 1);

    // Admin should be able to resolve the paused match.
    client.admin_resolve_stalled_match(&id, &admin, &Winner::Player2);

    let m = client.get_match(&id);
    assert_eq!(m.state, MatchState::Completed);
    assert_eq!(m.winner, Winner::Player2);
}

// ── #1517: loser front-runs oracle with rollback ──────────────────────────────

/// A single-player rollback call must NOT immediately refund. It only records the
/// vote. The refund runs only after both players have called (#1517 fix).
#[test]
fn test_1517_single_player_rollback_does_not_execute_refund() {
    let (env, contract_id, _oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);
    let tc = soroban_sdk::token::Client::new(&env, &token);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "gg77hh88");

    let p1_before = tc.balance(&player1);

    // Player1 calls rollback alone — must succeed but NOT transfer funds.
    client.dispute_and_rollback_match(
        &id,
        &player1,
        &String::from_str(&env, "disconnected"),
    );

    // Match is still Active (not Cancelled) — funds are still in escrow.
    let m = client.get_match(&id);
    assert_eq!(
        m.state,
        MatchState::Active,
        "match must stay Active after only one player votes for rollback"
    );
    assert_eq!(
        tc.balance(&player1),
        p1_before,
        "no refund should occur from a single-player rollback vote"
    );
    assert!(
        m.rollback_vote_player1,
        "player1's rollback vote must be recorded"
    );
}

/// Loser front-runs oracle with rollback: player1 votes, then the oracle
/// submits a result. The oracle settlement must win — funds go to the winner,
/// not refunded (#1517 fix).
#[test]
fn test_1517_loser_frontrun_rollback_oracle_settles_correctly() {
    let (env, contract_id, oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);
    let tc = soroban_sdk::token::Client::new(&env, &token);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "ii99jj00");

    let p2_before = tc.balance(&player2);

    // Loser (player1) votes for rollback trying to get their stake back.
    client.dispute_and_rollback_match(
        &id,
        &player1,
        &String::from_str(&env, "disconnect"),
    );

    // Oracle submits result before player2 consents — oracle wins.
    client.submit_result(&id, &Winner::Player2, &oracle);

    let m = client.get_match(&id);
    assert_eq!(m.state, MatchState::Completed);
    assert_eq!(m.winner, Winner::Player2);
    // Player2 received the full pot.
    assert_eq!(tc.balance(&player2), p2_before + 200);
}

/// Both players consenting to rollback executes the refund (#1517 positive case).
#[test]
fn test_1517_mutual_consent_rollback_refunds_both_players() {
    let (env, contract_id, _oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);
    let tc = soroban_sdk::token::Client::new(&env, &token);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "kk11ll22");

    let p1_before = tc.balance(&player1);
    let p2_before = tc.balance(&player2);

    // Both players agree to roll back.
    client.dispute_and_rollback_match(
        &id,
        &player1,
        &String::from_str(&env, "mutual agreement"),
    );
    client.dispute_and_rollback_match(
        &id,
        &player2,
        &String::from_str(&env, "mutual agreement"),
    );

    let m = client.get_match(&id);
    assert_eq!(m.state, MatchState::Cancelled);
    assert_eq!(tc.balance(&player1), p1_before + 100);
    assert_eq!(tc.balance(&player2), p2_before + 100);
}

// ── #1518: heartbeat-griefing keeps admin_resolve_stalled_match blocked ───────

/// A player heartbeating weekly must NOT be able to keep the admin stall
/// window from opening. `admin_resolve_stalled_match` now measures from
/// `activated_at`, not `last_heartbeat` (#1518 fix).
#[test]
fn test_1518_heartbeat_cannot_postpone_admin_resolve() {
    let (env, contract_id, _oracle, player1, player2, token, admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    env.ledger().set_timestamp(0);
    let id = make_active_match(&client, &env, &player1, &player2, &token, "mm33nn44");

    // Advance 6 days and have the loser heartbeat to try to reset the window.
    advance_time(&env, 6 * 24 * 60 * 60);
    client.heartbeat_match(&id, &player1);

    // Advance another 2 days — total elapsed from activation is 8 days,
    // but last_heartbeat is only 2 days old.
    advance_time(&env, 2 * 24 * 60 * 60);

    // Under the old logic (measuring from last_heartbeat) this would fail.
    // Under the new logic (measuring from activated_at = 8 days ago) it must succeed.
    client.admin_resolve_stalled_match(&id, &admin, &Winner::Draw);

    let m = client.get_match(&id);
    assert_eq!(m.state, MatchState::Completed);
}

/// Admin resolve before the 7-day stall window from activation must still fail.
#[test]
fn test_1518_admin_resolve_rejected_before_stall_window_from_activation() {
    let (env, contract_id, _oracle, player1, player2, token, admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    env.ledger().set_timestamp(0);
    let id = make_active_match(&client, &env, &player1, &player2, &token, "oo55pp66");

    // Only 3 days elapsed from activation.
    advance_time(&env, 3 * 24 * 60 * 60);

    let result = client.try_admin_resolve_stalled_match(&id, &admin, &Winner::Draw);
    assert_eq!(
        result,
        Err(Ok(Error::MatchNotExpired)),
        "admin resolve must be rejected when less than 7 days have passed since activation"
    );
}

// ── #1519: update_player_stats win/loss correctness ──────────────────────────

/// When Player1 wins, Player1's wins counter increments and Player2's losses counter
/// increments — not both wins (#1519 fix).
#[test]
fn test_1519_player1_win_increments_correct_counters() {
    let (env, contract_id, oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "qq77rr88");
    client.submit_result(&id, &Winner::Player1, &oracle);

    let p1_stats = client.get_player_stats(&player1);
    let p2_stats = client.get_player_stats(&player2);

    assert_eq!(p1_stats.wins, 1, "player1 must have 1 win");
    assert_eq!(p1_stats.losses, 0, "player1 must have 0 losses");
    assert_eq!(p2_stats.wins, 0, "player2 must have 0 wins");
    assert_eq!(p2_stats.losses, 1, "player2 must have 1 loss");
}

/// When Player2 wins, Player2's wins counter increments and Player1's losses counter
/// increments — not both losses (#1519 fix).
#[test]
fn test_1519_player2_win_increments_correct_counters() {
    let (env, contract_id, oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "ss99tt00");
    client.submit_result(&id, &Winner::Player2, &oracle);

    let p1_stats = client.get_player_stats(&player1);
    let p2_stats = client.get_player_stats(&player2);

    assert_eq!(p1_stats.wins, 0, "player1 must have 0 wins");
    assert_eq!(p1_stats.losses, 1, "player1 must have 1 loss");
    assert_eq!(p2_stats.wins, 1, "player2 must have 1 win");
    assert_eq!(p2_stats.losses, 0, "player2 must have 0 losses");
}

/// On a draw, both players' draws counter increments and neither wins nor loses.
#[test]
fn test_1519_draw_increments_draws_for_both_players() {
    let (env, contract_id, oracle, player1, player2, token, _admin) = setup();
    let client = EscrowContractClient::new(&env, &contract_id);

    let id = make_active_match(&client, &env, &player1, &player2, &token, "uu11vv22");
    client.submit_result(&id, &Winner::Draw, &oracle);

    let p1_stats = client.get_player_stats(&player1);
    let p2_stats = client.get_player_stats(&player2);

    assert_eq!(p1_stats.draws, 1, "player1 must have 1 draw");
    assert_eq!(p1_stats.wins, 0, "player1 must have 0 wins");
    assert_eq!(p1_stats.losses, 0, "player1 must have 0 losses");
    assert_eq!(p2_stats.draws, 1, "player2 must have 1 draw");
    assert_eq!(p2_stats.wins, 0, "player2 must have 0 wins");
    assert_eq!(p2_stats.losses, 0, "player2 must have 0 losses");
}
