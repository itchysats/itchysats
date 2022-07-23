use daemon::bdk::bitcoin::Amount;
use daemon::projection::CfdOrder;
use daemon::projection::CfdState;
use daemon::projection::MakerOffers;
use daemon_tests::confirm;
use daemon_tests::dummy_offer_params;
use daemon_tests::flow::is_next_offers_none;
use daemon_tests::flow::next_maker_offers;
use daemon_tests::flow::next_with;
use daemon_tests::flow::one_cfd_with_state;
use daemon_tests::start_both;
use daemon_tests::wait_next_state;
use model::Leverage;
use model::Position;
use model::Usd;
use otel_tests::otel_test;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio_extras::time::sleep;

#[otel_test]
async fn taker_receives_order_from_maker_on_publication() {
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

#[otel_test]
async fn taker_takes_order_and_maker_rejects() {
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let offer_id = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;
    let order_id = taker
        .system
        .place_order(offer_id, Usd::new(dec!(100)), Leverage::TWO)
        .await
        .unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::PendingSetup);

    maker.system.reject_order(order_id).await.unwrap();

    wait_next_state!(order_id, maker, taker, CfdState::Rejected);
}

#[otel_test]
async fn another_offer_is_automatically_created_after_taker_takes_order() {
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let offer_id = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;
    taker
        .system
        .place_order(offer_id, Usd::new(dec!(10)), Leverage::TWO)
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
        new_order_id_taker, offer_id,
        "Another offer should be available, and it should have a different id than first one"
    );

    assert_ne!(
        new_order_id_maker, offer_id,
        "Another offer should be available, and it should have a different id than first one"
    );

    assert_eq!(
        new_order_id_taker, new_order_id_maker,
        "Both parties have the same new id"
    )
}

#[otel_test]
async fn taker_takes_order_and_maker_accepts_and_contract_setup() {
    let (mut maker, mut taker) = start_both().await;

    is_next_offers_none(taker.offers_feed()).await.unwrap();

    maker
        .set_offer_params(dummy_offer_params(Position::Short))
        .await;

    let (_, received) = next_maker_offers(maker.offers_feed(), taker.offers_feed())
        .await
        .unwrap();

    let offer_id = received.short.unwrap().id;

    taker.mocks.mock_oracle_announcement().await;
    maker.mocks.mock_oracle_announcement().await;

    let order_id = taker
        .system
        .place_order(offer_id, Usd::new(dec!(100)), Leverage::TWO)
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
