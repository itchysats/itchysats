use crate::collab_settlement;
use crate::collab_settlement::taker::Settle;
use crate::order;
use crate::projection;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use model::libp2p::PeerId;
use model::market_closing_price;
use model::Cfd;
use model::Leverage;
use model::MakerOffers;
use model::OrderId;
use model::Origin;
use model::Price;
use model::Role;
use model::Usd;
use sqlite_db;
use time::OffsetDateTime;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

#[derive(Clone)]
pub struct CurrentMakerOffers(pub Option<MakerOffers>);

#[derive(Clone, Copy)]
pub struct PlaceOrder {
    pub offer_id: OrderId,
    pub quantity: Usd,
    pub leverage: Leverage,
}

#[derive(Clone)]
pub struct ProposeSettlement {
    pub order_id: OrderId,
    pub bid: Price,
    pub ask: Price,
    pub quote_timestamp: String,
}

pub struct Actor {
    db: sqlite_db::Connection,
    projection_actor: xtra::Address<projection::Actor>,
    libp2p_collab_settlement_actor: xtra::Address<collab_settlement::taker::Actor>,
    order_actor: xtra::Address<order::taker::Actor>,
    current_maker_offers: Option<MakerOffers>,
    maker_peer_id: PeerId,
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: sqlite_db::Connection,
        projection_actor: xtra::Address<projection::Actor>,
        libp2p_collab_settlement_actor: xtra::Address<collab_settlement::taker::Actor>,
        order_actor: xtra::Address<order::taker::Actor>,
        maker_peer_id: PeerId,
    ) -> Self {
        Self {
            db,
            projection_actor,
            libp2p_collab_settlement_actor,
            order_actor,
            current_maker_offers: None,
            maker_peer_id,
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_current_offers(&mut self, msg: xtra_libp2p_offer::taker::LatestMakerOffers) {
        let takers_perspective_of_maker_offers = msg.0.map(|mut maker_offers| {
            maker_offers.long = maker_offers.long.map(|mut long| {
                long.origin = Origin::Theirs;
                long
            });
            maker_offers.short = maker_offers.short.map(|mut short| {
                short.origin = Origin::Theirs;
                short
            });

            maker_offers
        });

        self.current_maker_offers = takers_perspective_of_maker_offers.clone();
        tracing::trace!("new maker offers {:?}", takers_perspective_of_maker_offers);

        if let Err(e) = self
            .projection_actor
            .send(projection::Update(
                takers_perspective_of_maker_offers.clone(),
            ))
            .await
        {
            tracing::warn!("Failed to send current offers to projection actor: {e:#}");
        };
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_propose_settlement(&mut self, msg: ProposeSettlement) -> Result<()> {
        let ProposeSettlement {
            order_id,
            bid,
            ask,
            quote_timestamp,
        } = msg;

        let cfd = self.db.load_open_cfd::<Cfd>(order_id, ()).await?;

        let proposal_closing_price = market_closing_price(bid, ask, Role::Taker, cfd.position());

        tracing::debug!(%order_id, %proposal_closing_price, %bid, %ask, %quote_timestamp, "Proposing settlement of contract");

        // Wait for the response to check for invariants (ie. whether it is possible to settle)
        self.libp2p_collab_settlement_actor
            .send(Settle {
                order_id,
                price: proposal_closing_price,
                maker_peer_id: cfd
                    .counterparty_peer_id()
                    .context("No counterparty peer id found")?,
            })
            .await??;

        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, msg: PlaceOrder) -> Result<OrderId> {
        let PlaceOrder {
            offer_id,
            quantity,
            leverage,
        } = msg;

        let offer = self
            .current_maker_offers
            .clone()
            .context("No maker offers available to take")?
            .take_order(offer_id)
            .0
            .context("Offer to take could not be found in current maker offers, you might have an outdated offer")?;

        // TODO: Consider removing this. Why can't we just place the
        // order we want, if it's the want we selected in the UI? The
        // maker should simply reject our request to place an order if
        // it's outdated
        if !offer.is_safe_to_take(OffsetDateTime::now_utc()) {
            bail!("The maker's offer appears to be outdated, refusing to place order");
        }

        let order_id = OrderId::default();
        let place_order = order::taker::PlaceOrder::new(
            order_id,
            (quantity, leverage, offer.position_maker),
            offer.price,
            offer.settlement_interval,
            (offer.opening_fee, offer.funding_rate, offer.tx_fee_rate),
            offer.oracle_event_id,
            self.maker_peer_id.inner(),
        );

        self.order_actor
            .send_async_safe(place_order)
            .await
            .context("Failed to place order")?;

        Ok(order_id)
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}
