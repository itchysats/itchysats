use anyhow::Context;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::bitcoin::SignedAmount;
use daemon::bdk::bitcoin::Txid;
use daemon::connection::ConnectionStatus;
use daemon::projection::CfdOrder;
use daemon::projection::CfdState;
use daemon::projection::MakerOffers;
use daemon_tests::dummy_offer_params;
use daemon_tests::dummy_quote;
use daemon_tests::flow::is_next_offers_none;
use daemon_tests::flow::next;
use daemon_tests::flow::next_maker_offers;
use daemon_tests::flow::next_with;
use daemon_tests::flow::one_cfd_with_state;
use daemon_tests::init_tracing;
use daemon_tests::maia::OliviaData;
use daemon_tests::mock_oracle_announcements;
use daemon_tests::mocks::oracle::dummy_wrong_attestation;
use daemon_tests::simulate_attestation;
use daemon_tests::start_both;
use daemon_tests::wait_next_state;
use daemon_tests::Maker;
use daemon_tests::MakerConfig;
use daemon_tests::Taker;
use daemon_tests::TakerConfig;
use maker::cfd::OfferParams;
use model::olivia;
use model::olivia::BitMexPriceEventId;
use model::FeeAccount;
use model::FundingFee;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Role;
use model::Usd;
use model::SETTLEMENT_INTERVAL;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio_extras::time::sleep;

macro_rules! confirm {
    (lock transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_lock_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_lock_transaction($id)
            .await;
    };
    (commit transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_commit_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_commit_transaction($id)
            .await;
    };
    (refund transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_refund_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_refund_transaction($id)
            .await;
    };
    (close transaction, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .confirm_close_transaction($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .confirm_close_transaction($id)
            .await;
    };
    (cet, $id:expr, $maker:expr, $taker:expr) => {
        $maker.mocks.monitor().await.confirm_cet($id).await;
        $taker.mocks.monitor().await.confirm_cet($id).await;
    };
}

macro_rules! expire {
    (cet timelock, $id:expr, $maker:expr, $taker:expr) => {
        $maker.mocks.monitor().await.expire_cet_timelock($id).await;
        $taker.mocks.monitor().await.expire_cet_timelock($id).await;
    };
    (refund timelock, $id:expr, $maker:expr, $taker:expr) => {
        $maker
            .mocks
            .monitor()
            .await
            .expire_refund_timelock($id)
            .await;
        $taker
            .mocks
            .monitor()
            .await
            .expire_refund_timelock($id)
            .await;
    };
}

#[tokio::test]
async fn taker_receives_order_from_maker_on_publication() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    assert!(is_next_offers_none(taker.offers_feed()).await.unwrap());

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (published, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    assert_eq_offers(published, received);
}

fn assert_eq_offers(published: MakerOffers, received: MakerOffers) {
    match (published.long, received.long) {
        (None, None) => (),
        (Some(published), Some(received)) => assert_eq_orders(published, received),
        (None, Some(_)) => panic!("Long orders did not match: Published None, received Some "),
        (Some(_), None) => panic!("Long orders did not match: Published Some, received None "),
    }

    match (published.short, received.short) {
        (None, None) => (),
        (Some(published), Some(received)) => assert_eq_orders(published, received),
        (None, Some(_)) => panic!("Short orders did not match: Published None, received Some "),
        (Some(_), None) => panic!("Short orders did not match: Published Some, received None "),
    }
}

fn assert_eq_orders(mut published: CfdOrder, received: CfdOrder) {
    // we fix the leverage to TWO for our test
    let fixed_leverage = Leverage::TWO;

    // make sure that the initial funding fee per lot is flipped
    // note: we publish as maker and receive as taker, the funding fee is to be received by one
    // party and paid by the other

    assert_eq!(
        published
            .leverage_details
            .iter()
            .find(|l| l.leverage == fixed_leverage)
            .unwrap()
            .initial_funding_fee_per_lot,
        received
            .leverage_details
            .iter()
            .find(|l| l.leverage == fixed_leverage)
            .unwrap()
            .initial_funding_fee_per_lot
            * -1
    );
    // align leverage details so we can assert on the order
    published.leverage_details = received.leverage_details.clone();

    assert_eq!(published, received);

    // Hard-coded to match the dummy_new_order()
    assert_eq!(received.opening_fee.unwrap(), Amount::from_sat(2));
    assert_eq!(received.funding_rate_hourly_percent, "0.00100");
}

#[tokio::test]
async fn taker_takes_order_and_maker_rejects() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let order_id = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;
    taker
        .system
        .take_offer(order_id, Usd::new(dec!(100)), Leverage::TWO)
        .await
        .unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::PendingSetup);

    maker.system.reject_order(order_id).await.unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::Rejected);
}

#[tokio::test]
async fn another_offer_is_automatically_created_after_taker_takes_order() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let order_id_take = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;
    taker
        .system
        .take_offer(order_id_take, Usd::new(dec!(10)), Leverage::TWO)
        .await
        .unwrap();

    // For the taker the offer is reset after taking it, so we can't take the same one twice
    is_next_offers_none(taker.offers_feed()).await.unwrap();

    let (maker_update, taker_update) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let new_order_id_maker = maker_update.short.unwrap().id;
    let new_order_id_taker = taker_update.short.unwrap().id;

    assert_ne!(
        new_order_id_taker, order_id_take,
        "Another offer should be available, and it should have a different id than first one"
    );

    assert_ne!(
        new_order_id_maker, order_id_take,
        "Another offer should be available, and it should have a different id than first one"
    );

    assert_eq!(
        new_order_id_taker, new_order_id_maker,
        "Both parties have the same new id"
    )
}

#[tokio::test]
async fn taker_takes_order_and_maker_accepts_and_contract_setup() {
    let _guard = init_tracing();
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let order_id = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;

    taker
        .system
        .take_offer(order_id, Usd::new(dec!(100)), Leverage::TWO)
        .await
        .unwrap();
    wait_next_state!(order_id, maker, taker, CfdState::PendingSetup);

    maker.mocks.mock_party_params().await;
    taker.mocks.mock_party_params().await;

    maker.mocks.mock_wallet_sign_and_broadcast().await;
    taker.mocks.mock_wallet_sign_and_broadcast().await;

    maker.system.accept_order(order_id).await.unwrap();
    wait_next_state!(order_id, maker, taker, CfdState::ContractSetup);

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::PendingOpen);

    confirm!(lock transaction, order_id, maker, taker);
    wait_next_state!(order_id, maker, taker, CfdState::Open);
}

#[tokio::test]
async fn collaboratively_close_an_open_cfd_maker_going_short() {
    let _guard = init_tracing();
    collaboratively_close_an_open_cfd(Position::Short).await;
}

#[tokio::test]
async fn collaboratively_close_an_open_cfd_maker_going_long() {
    let _guard = init_tracing();
    collaboratively_close_an_open_cfd(Position::Long).await;
}

async fn collaboratively_close_an_open_cfd(maker_position: Position) {
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(OliviaData::example_0().announcements(), maker_position).await;
    taker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    maker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    next_with(taker.quote_feed(), |q| q).await.unwrap(); // if quote is available on feed, it propagated through the system

    taker.system.propose_settlement(order_id).await.unwrap();

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingSettlementProposal,
        CfdState::OutgoingSettlementProposal
    );

    maker.system.accept_settlement(order_id).await.unwrap();
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition

    wait_next_state!(order_id, maker, taker, CfdState::PendingClose);
    confirm!(close transaction, order_id, maker, taker);

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition

    wait_next_state!(order_id, maker, taker, CfdState::Closed);
}

#[tokio::test]
async fn force_close_an_open_cfd_maker_going_short() {
    let _guard = init_tracing();
    force_close_open_cfd(Position::Short).await;
}

#[tokio::test]
async fn force_close_an_open_cfd_maker_going_long() {
    let _guard = init_tracing();
    force_close_open_cfd(Position::Long).await;
}

async fn force_close_open_cfd(maker_position: Position) {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcements(), maker_position).await;
    // Taker initiates force-closing
    taker.system.commit(order_id).await.unwrap();

    confirm!(commit transaction, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);

    // After CetTimelockExpired, we're only waiting for attestation
    expire!(cet timelock, order_id, maker, taker);

    // Delivering the wrong attestation does not move state to `PendingCet`
    simulate_attestation!(taker, maker, order_id, dummy_wrong_attestation());

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);

    // Delivering correct attestation moves the state `PendingCet`
    simulate_attestation!(
        taker,
        maker,
        order_id,
        oracle_data.attestations()[0].clone()
    );

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::PendingCet);

    confirm!(cet, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::Closed);
}

#[tokio::test]
async fn rollover_an_open_cfd_maker_going_short() {
    let _guard = init_tracing();
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

#[tokio::test]
async fn rollover_an_open_cfd_maker_going_long() {
    let _guard = init_tracing();
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

#[tokio::test]
async fn double_rollover_an_open_cfd() {
    // double rollover ensures that both parties properly succeeded and can do another rollover

    let _guard = init_tracing();
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
/// This test simulates a rollover retry
///
/// We use two different oracle events: `exmaple_0` and `example_1`
/// The contract setup is done with `example_0`.
/// The first rollover is done with `example_1`.
/// The second rollover is done with `example_0` (we re-use it)
#[tokio::test]
async fn retry_rollover_an_open_cfd() {
    let _guard = init_tracing();

    let contract_setup_oracle_data = OliviaData::example_0();
    let contract_setup_oracle_data_announcements = contract_setup_oracle_data.announcements();
    let (mut maker, mut taker, order_id, fee_structure) =
        prepare_rollover(Position::Short, contract_setup_oracle_data.clone()).await;

    let taker_commit_txid_after_contract_setup = taker.latest_commit_txid();

    let first_rollover_oracle_data = OliviaData::example_1();
    let first_rollover_oracle_data_announcements = first_rollover_oracle_data.announcements();

    // We mock a different oracle event-id for the first rollover
    mock_oracle_announcements(
        &mut maker,
        &mut taker,
        first_rollover_oracle_data_announcements.clone(),
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

    let taker_commit_txid_after_first_rollover = taker.latest_commit_txid();

    // We ensure that the taker has a *different* commit_txid after the first rollover
    assert_ne!(
        taker_commit_txid_after_contract_setup,
        taker_commit_txid_after_first_rollover,
        "The commit_txid of the taker after the first rollover should not be the same as after contract setup"
    );

    let maker_commit_txid_after_first_rollover = maker.latest_commit_txid();

    // We ensure that maker and taker have the same commit_txid after the first rollover
    assert_eq!(
        taker_commit_txid_after_first_rollover, maker_commit_txid_after_first_rollover,
        "The maker and taker should have the same commit_txid after the first rollover"
    );

    // We mock the initial oracle event-id for the second rollover again
    mock_oracle_announcements(
        &mut maker,
        &mut taker,
        contract_setup_oracle_data_announcements.clone(),
    )
    .await;

    // We expect that the second rollover won't add additional costs, because it is a retry
    let (expected_maker_fee, expected_taker_fee) = fee_structure.predict_fees(24);
    // We explicitly specify the `from_params_taker` for the second rollover which results in a
    // retry of the first rollover, i.e. the taker specifies the given `commit_txid` when proposing
    // the rollover to the maker.
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        contract_setup_oracle_data,
        Some((
            taker_commit_txid_after_contract_setup,
            contract_setup_oracle_data_announcements.last().unwrap().id,
        )),
        expected_maker_fee,
        expected_taker_fee,
    )
    .await;

    let taker_commit_txid_after_second_rollover = taker.latest_commit_txid();

    // We ensure that the taker has a *different* commit_txid after the second rollover
    assert_ne!(
        taker_commit_txid_after_first_rollover,
        taker_commit_txid_after_second_rollover,
        "The commit_txid of the taker after the second rollover should not be the same as after the first rollover"
    );

    let maker_commit_txid_after_second_rollover = maker.latest_commit_txid();

    // We ensure that maker and taker have the same commit_txid after the second rollover
    assert_eq!(
        taker_commit_txid_after_second_rollover, maker_commit_txid_after_second_rollover,
        "The maker and taker should have the same commit_txid after the second rollover"
    );
}

async fn prepare_rollover(
    maker_position: Position,
    oracle_data: OliviaData,
) -> (Maker, Taker, OrderId, FeeStructure) {
    let (mut maker, mut taker, order_id, fee_structure) =
        start_from_open_cfd_state(oracle_data.announcements(), maker_position).await;

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
        oracle_data.announcements().last().unwrap().id,
        maker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id"
    );
    assert_eq!(
        oracle_data.announcements().last().unwrap().id,
        taker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id"
    );
}

#[tokio::test]
async fn maker_rejects_rollover_of_open_cfd() {
    let _guard = init_tracing();
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcements(), Position::Short).await;

    let is_accepting_rollovers = false;
    maker
        .system
        .update_rollover_configuration(is_accepting_rollovers)
        .await
        .unwrap();

    taker
        .trigger_rollover_with_latest_dlc_params(order_id)
        .await;

    wait_next_state!(order_id, maker, taker, CfdState::Open);
}

#[tokio::test]
async fn maker_rejects_collab_settlement_after_commit_finality() {
    let _guard = init_tracing();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(OliviaData::example_0().announcements(), Position::Short).await;
    taker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    maker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    next_with(taker.quote_feed(), |q| q).await.unwrap(); // if quote is available on feed, it propagated through the system

    taker.system.propose_settlement(order_id).await.unwrap();

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingSettlementProposal,
        CfdState::OutgoingSettlementProposal
    );

    confirm!(commit transaction, order_id, maker, taker);

    maker.system.reject_settlement(order_id).await.unwrap();
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition

    // After rejecting rollover, we should display where we were before the
    // rollover attempt
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);
}

#[tokio::test]
async fn maker_accepts_collab_settlement_after_commit_finality() {
    let _guard = init_tracing();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(OliviaData::example_0().announcements(), Position::Short).await;
    taker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    maker.mocks.mock_latest_quote(Some(dummy_quote())).await;
    next_with(taker.quote_feed(), |q| q).await.unwrap(); // if quote is available on feed, it propagated through the system

    taker.system.propose_settlement(order_id).await.unwrap();

    wait_next_state!(
        order_id,
        maker,
        taker,
        CfdState::IncomingSettlementProposal,
        CfdState::OutgoingSettlementProposal
    );

    confirm!(commit transaction, order_id, maker, taker);

    maker.system.accept_settlement(order_id).await.unwrap();
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition

    // After rejecting rollover, we should display where we were before the
    // rollover attempt
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);
}

#[tokio::test]
async fn open_cfd_is_refunded() {
    let _guard = init_tracing();
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcements(), Position::Short).await;
    confirm!(commit transaction, order_id, maker, taker);

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);

    expire!(refund timelock, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::PendingRefund);

    confirm!(refund transaction, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::Refunded);
}

#[tokio::test]
async fn taker_notices_lack_of_maker() {
    let _guard = init_tracing();

    let maker_config = MakerConfig::default()
        .with_dedicated_port(35123)
        .with_dedicated_libp2p_port(35124); // set fixed ports so the taker can reconnect
    let maker = Maker::start(&maker_config).await;

    let taker_config = TakerConfig::default();
    let mut taker = Taker::start(
        &taker_config,
        maker.listen_addr,
        maker.identity,
        maker.connect_addr.clone(),
    )
    .await;

    sleep(Duration::from_secs(5)).await; // wait a bit until taker notices change

    assert_eq!(
        ConnectionStatus::Online,
        next(taker.maker_status_feed()).await.unwrap()
    );

    drop(maker);

    sleep(Duration::from_secs(5)).await; // wait a bit until taker notices change

    assert_eq!(
        ConnectionStatus::Offline { reason: None },
        next(taker.maker_status_feed()).await.unwrap(),
    );

    let _maker = Maker::start(&maker_config).await;

    sleep(Duration::from_secs(5)).await; // wait a bit until taker notices change

    assert_eq!(
        ConnectionStatus::Online,
        next(taker.maker_status_feed()).await.unwrap(),
    );
}

#[tokio::test]
async fn maker_notices_lack_of_taker() {
    let _guard = init_tracing();

    let (mut maker, taker) = start_both().await;
    assert_eq!(
        vec![taker.id],
        next(maker.connected_takers_feed()).await.unwrap()
    );

    drop(taker);

    assert_eq!(
        Vec::<Identity>::new(),
        next(maker.connected_takers_feed()).await.unwrap()
    );
}
pub struct FeeStructure {
    /// Opening fee charged by the maker
    opening_fee: OpeningFee,

    /// Funding fee for the first 24h calculated when opening a Cfd
    initial_funding_fee: FundingFee,

    /// The maker's position for the Cfd
    maker_position: Position,

    offer_params: OfferParams,
    quantity: Usd,
    taker_leverage: Leverage,
}

impl FeeStructure {
    pub fn new(
        offer_params: OfferParams,
        quantity: Usd,
        taker_leverage: Leverage,
        maker_position: Position,
    ) -> Self {
        let initial_funding_fee = match maker_position {
            Position::Long => FundingFee::calculate(
                offer_params.price_long.unwrap(),
                quantity,
                Leverage::ONE,
                taker_leverage,
                offer_params.funding_rate_long,
                SETTLEMENT_INTERVAL.whole_hours(),
            )
            .unwrap(),
            Position::Short => FundingFee::calculate(
                offer_params.price_short.unwrap(),
                quantity,
                taker_leverage,
                Leverage::ONE,
                offer_params.funding_rate_short,
                SETTLEMENT_INTERVAL.whole_hours(),
            )
            .unwrap(),
        };

        Self {
            opening_fee: offer_params.opening_fee,
            initial_funding_fee,
            maker_position,
            offer_params,
            quantity,
            taker_leverage,
        }
    }

    pub fn predict_fees(
        &self,
        accumulated_rollover_hours_to_charge: i64,
    ) -> (SignedAmount, SignedAmount) {
        if accumulated_rollover_hours_to_charge == 0 {
            tracing::info!("Predicting fees before first rollover")
        } else {
            tracing::info!(
                "Predicting fee for {} hours",
                accumulated_rollover_hours_to_charge
            );
        }

        tracing::debug!("Opening fee: {}", self.opening_fee);

        let mut maker_fee_account = FeeAccount::new(self.maker_position, Role::Maker)
            .add_opening_fee(self.opening_fee)
            .add_funding_fee(self.initial_funding_fee);

        let taker_position = self.maker_position.counter_position();
        let mut taker_fee_account = FeeAccount::new(taker_position, Role::Taker)
            .add_opening_fee(self.opening_fee)
            .add_funding_fee(self.initial_funding_fee);

        tracing::debug!(
            "Maker fees including opening and initial funding fee: {}",
            maker_fee_account.balance()
        );

        tracing::debug!(
            "Taker fees including opening and initial funding fee: {}",
            taker_fee_account.balance()
        );

        let accumulated_hours_to_charge = match self.maker_position {
            Position::Long => FundingFee::calculate(
                self.offer_params.price_long.unwrap(),
                self.quantity,
                Leverage::ONE,
                self.taker_leverage,
                self.offer_params.funding_rate_long,
                accumulated_rollover_hours_to_charge,
            )
            .unwrap(),
            Position::Short => FundingFee::calculate(
                self.offer_params.price_short.unwrap(),
                self.quantity,
                self.taker_leverage,
                Leverage::ONE,
                self.offer_params.funding_rate_short,
                accumulated_rollover_hours_to_charge,
            )
            .unwrap(),
        };

        maker_fee_account = maker_fee_account.add_funding_fee(accumulated_hours_to_charge);
        taker_fee_account = taker_fee_account.add_funding_fee(accumulated_hours_to_charge);

        tracing::debug!(
            "Maker fees including all fees: {}",
            maker_fee_account.balance()
        );

        tracing::debug!(
            "Taker fees including all fees: {}",
            taker_fee_account.balance()
        );

        (maker_fee_account.balance(), taker_fee_account.balance())
    }
}

/// Hide the implementation detail of arriving at the Cfd open state.
/// Useful when reading tests that should start at this point.
/// For convenience, returns also OrderId of the opened Cfd.
/// `announcement` is used during Cfd's creation.
async fn start_from_open_cfd_state(
    announcements: Vec<olivia::Announcement>,
    position_maker: Position,
) -> (Maker, Taker, OrderId, FeeStructure) {
    let mut maker = Maker::start(&MakerConfig::default()).await;
    let mut taker = Taker::start(
        &TakerConfig::default(),
        maker.listen_addr,
        maker.identity,
        maker.connect_addr.clone(),
    )
    .await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    let offer_params = dummy_offer_params(position_maker);

    let quantity = Usd::new(dec!(100));
    let taker_leverage = Leverage::TWO;

    let fee_structure = FeeStructure::new(
        offer_params.clone(),
        quantity,
        taker_leverage,
        position_maker,
    );

    maker.set_offer_params(offer_params).await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    taker
        .mocks
        .mock_oracle_announcement_with(announcements.clone())
        .await;
    maker
        .mocks
        .mock_oracle_announcement_with(announcements)
        .await;

    let order_to_take = match position_maker {
        Position::Short => received.short,
        Position::Long => received.long,
    }
    .context("Order for expected position not set")
    .unwrap();

    taker
        .system
        .take_offer(order_to_take.id, quantity, taker_leverage)
        .await
        .unwrap();

    wait_next_state!(order_to_take.id, maker, taker, CfdState::PendingSetup);

    maker.mocks.mock_party_params().await;
    taker.mocks.mock_party_params().await;

    maker.mocks.mock_wallet_sign_and_broadcast().await;
    taker.mocks.mock_wallet_sign_and_broadcast().await;

    maker.system.accept_order(order_to_take.id).await.unwrap();
    wait_next_state!(order_to_take.id, maker, taker, CfdState::ContractSetup);

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_to_take.id, maker, taker, CfdState::PendingOpen);

    confirm!(lock transaction, order_to_take.id, maker, taker);
    wait_next_state!(order_to_take.id, maker, taker, CfdState::Open);

    (maker, taker, order_to_take.id, fee_structure)
}
