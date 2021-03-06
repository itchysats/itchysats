use daemon::bdk::bitcoin::SignedAmount;
use daemon::bdk::bitcoin::Txid;
use daemon::projection::CfdState;
use daemon_tests::confirm;
use daemon_tests::dummy_offer_params;
use daemon_tests::dummy_quote;
use daemon_tests::flow::next_with;
use daemon_tests::flow::one_cfd_with_state;
use daemon_tests::maia::OliviaData;
use daemon_tests::mock_oracle_announcements;
use daemon_tests::start_from_open_cfd_state;
use daemon_tests::wait_next_state;
use daemon_tests::FeeStructure;
use daemon_tests::Maker;
use daemon_tests::Taker;
use model::olivia::BitMexPriceEventId;
use model::OrderId;
use model::Position;
use otel_tests::otel_test;

#[otel_test]
async fn rollover_an_open_cfd_maker_going_short() {
    let (mut maker, mut taker, order_id, fee_structure) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;
}

#[otel_test]
async fn rollover_an_open_cfd_maker_going_long() {
    let (mut maker, mut taker, order_id, fee_structure) =
        prepare_rollover(Position::Long, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;
}

#[otel_test]
async fn double_rollover_an_open_cfd() {
    // double rollover ensures that both parties properly succeeded and can do another rollover

    let (mut maker, mut taker, order_id, fee_structure) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;

    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(48);
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;
}

#[otel_test]
async fn maker_rejects_rollover_of_open_cfd() {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcement(), Position::Short).await;

    taker
        .trigger_rollover_with_latest_dlc_params(order_id)
        .await;

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingRolloverProposal,
        CfdState::OutgoingRolloverProposal
    );

    maker.system.reject_rollover(order_id).await.unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::Open);
}

#[otel_test]
async fn maker_rejects_rollover_after_commit_finality() {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcement(), Position::Short).await;

    taker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    maker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    next_with(taker.quote_feed(), |q| q).await.unwrap(); // if quote is available on feed, it propagated through the system

    taker
        .trigger_rollover_with_latest_dlc_params(order_id)
        .await;

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingRolloverProposal,
        CfdState::OutgoingRolloverProposal
    );

    confirm!(commit transaction, order_id, maker, taker);
    // Cfd would be in "OpenCommitted" if it wasn't for the rollover

    maker.system.reject_rollover(order_id).await.unwrap();

    // After rejecting rollover, we should display where we were before the
    // rollover attempt
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);
}

#[otel_test]
async fn maker_accepts_rollover_after_commit_finality() {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcement(), Position::Short).await;

    taker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    maker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    next_with(taker.quote_feed(), |q| q).await.unwrap(); // if quote is available on feed, it propagated through the system

    taker
        .trigger_rollover_with_latest_dlc_params(order_id)
        .await;

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingRolloverProposal,
        CfdState::OutgoingRolloverProposal
    );

    confirm!(commit transaction, order_id, maker, taker);

    maker.system.accept_rollover(order_id).await.unwrap(); // This should fail

    wait_next_state!(
        order_id,
        maker,
        taker,
        // FIXME: Maker wrongly changes state even when rollover does not happen
        CfdState::RolloverSetup,
        CfdState::OpenCommitted
    );
}

/// This test simulates a rollover retry
///
/// We use two different oracle events: `exmaple_0` and `example_1`
/// The contract setup is done with `example_0`.
/// The first rollover is done with `example_1`.
/// The second rollover is done with `example_0` (we re-use it)
#[otel_test]
async fn retry_rollover_an_open_cfd() {
    let contract_setup_oracle_data = OliviaData::example_0();
    let contract_setup_oracle_data_announcement = contract_setup_oracle_data.announcement();
    let (mut maker, mut taker, order_id, fee_structure) =
        prepare_rollover(Position::Short, contract_setup_oracle_data.clone()).await;

    let taker_commit_txid_after_contract_setup = taker.latest_commit_txid();
    let taker_dlc_after_contract_setup = taker.latest_dlc();
    let taker_complete_fee_after_contract_setup = taker.latest_fees();

    let first_rollover_oracle_data = OliviaData::example_1();
    let first_rollover_oracle_data_announcement = first_rollover_oracle_data.announcement();

    // We mock a different oracle event-id for the first rollover
    mock_oracle_announcements(
        &mut maker,
        &mut taker,
        first_rollover_oracle_data_announcement.clone(),
    )
    .await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        first_rollover_oracle_data,
        None,
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;

    // We simulate the taker being one rollover behind by setting the
    // latest DLC to the one generated by contract setup
    taker
        .simulate_previous_rollover(
            order_id,
            taker_dlc_after_contract_setup,
            taker_complete_fee_after_contract_setup,
        )
        .await;

    // We mock the initial oracle event-id for the rollover retry again
    mock_oracle_announcements(
        &mut maker,
        &mut taker,
        contract_setup_oracle_data_announcement.clone(),
    )
    .await;

    // We expect that the rollover retry won't add additional costs
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);

    // The taker proposes a rollover starting from the DLC that
    // corresponds to contract setup
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        contract_setup_oracle_data,
        Some((
            taker_commit_txid_after_contract_setup,
            contract_setup_oracle_data_announcement.id,
        )),
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;

    assert_ne!(
        taker_commit_txid_after_contract_setup,
        taker.latest_commit_txid(),
        "The commit_txid of the taker after the rollover retry should have changed"
    );

    assert_eq!(
        taker.latest_commit_txid(),
        maker.latest_commit_txid(),
        "The maker and taker should have the same commit_txid after the rollover retry"
    );
}

async fn prepare_rollover(
    maker_position: Position,
    oracle_data: OliviaData,
) -> (Maker, Taker, OrderId, FeeStructure) {
    let (mut maker, mut taker, order_id, fee_structure) =
        start_from_open_cfd_state(oracle_data.announcement(), maker_position).await;

    // Maker needs to have an active offer in order to accept rollover
    maker
        .set_offer_params(dummy_offer_params(maker_position))
        .await;

    let maker_cfd = maker.first_cfd();
    let taker_cfd = taker.first_cfd();

    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(0);
    assert_eq!(expected_maker_fee, maker_cfd.accumulated_fees);
    assert_eq!(expected_taker_fee, taker_cfd.accumulated_fees);

    (maker, taker, order_id, fee_structure)
}

async fn rollover(
    maker: &mut Maker,
    taker: &mut Taker,
    order_id: OrderId,
    oracle_data: OliviaData,
    from_params_taker: Option<(Txid, BitMexPriceEventId)>,
    expected_fees_after_rollover_maker: SignedAmount,
    expected_fees_after_rollover_taker: SignedAmount,
) {
    match from_params_taker {
        None => {
            taker
                .trigger_rollover_with_latest_dlc_params(order_id)
                .await;
        }
        Some((from_commit_txid, from_settlement_event_id)) => {
            taker
                .trigger_rollover_with_specific_params(
                    order_id,
                    from_commit_txid,
                    from_settlement_event_id,
                )
                .await;
        }
    }

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingRolloverProposal,
        CfdState::OutgoingRolloverProposal
    );

    maker.system.accept_rollover(order_id).await.unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::RolloverSetup);
    wait_next_state!(order_id, maker, taker, CfdState::Open);

    let maker_cfd = maker.first_cfd();
    let taker_cfd = taker.first_cfd();

    assert_eq!(
        expected_fees_after_rollover_maker, maker_cfd.accumulated_fees,
        "Maker's fees after rollover don't match predicted fees"
    );
    assert_eq!(
        expected_fees_after_rollover_taker, taker_cfd.accumulated_fees,
        "Taker's fees after rollover don't match predicted fees"
    );

    // Ensure that the event ID of the latest dlc is the event ID used for rollover
    assert_eq!(
        oracle_data.announcement().id,
        maker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id"
    );
    assert_eq!(
        oracle_data.announcement().id,
        taker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id"
    );
}
