use daemon::bdk;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::bitcoin::Network;
use daemon::bdk::blockchain::ElectrumBlockchain;
use daemon::bdk::sled;
use daemon::connection::ConnectionStatus;
use daemon::oracle;
use daemon::projection;
use daemon::projection::CfdAction;
use daemon::projection::Feeds;
use daemon::wallet;
use daemon::TakerActorSystem;
use http_api_problem::HttpApiProblem;
use http_api_problem::StatusCode;
use model::Leverage;
use model::OrderId;
use model::Price;
use model::Timestamp;
use model::Usd;
use model::WalletInfo;
use rocket::http::ContentType;
use rocket::http::Status;
use rocket::response::stream::Event;
use rocket::response::stream::EventStream;
use rocket::response::Responder;
use rocket::serde::json::Json;
use rocket::serde::uuid::Uuid;
use rocket::State;
use rocket_basicauth::Authenticated;
use rust_embed::RustEmbed;
use rust_embed_rocket::EmbeddedFileExt;
use serde::Deserialize;
use serde::Serialize;
use shared_bin::ToSseEvent;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::select;
use tokio::sync::watch;
use tracing::instrument;

type Taker = TakerActorSystem<
    oracle::Actor,
    wallet::Actor<ElectrumBlockchain, sled::Tree>,
    xtra_bitmex_price_feed::Actor,
>;

const HEARTBEAT_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize)]
pub struct IdentityInfo {
    /// legacy networking identity
    pub(crate) taker_id: String,
    /// libp2p peer id
    pub(crate) taker_peer_id: String,
}

#[rocket::get("/feed")]
#[instrument(name = "GET /feed", skip_all)]
pub async fn feed(
    rx: &State<Feeds>,
    rx_wallet: &State<watch::Receiver<Option<WalletInfo>>>,
    rx_maker_status: &State<watch::Receiver<ConnectionStatus>>,
    identity_info: &State<IdentityInfo>,
    _auth: Authenticated,
) -> EventStream![] {
    let rx = rx.inner();
    let mut rx_cfds = rx.cfds.clone();
    let mut rx_offers = rx.offers.clone();
    let mut rx_quote = rx.quote.clone();
    let mut rx_wallet = rx_wallet.inner().clone();
    let mut rx_maker_status = rx_maker_status.inner().clone();
    let identity = identity_info.inner().clone();
    let mut heartbeat =
        tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));

    EventStream! {
        let wallet_info = rx_wallet.borrow().clone();
        yield wallet_info.to_sse_event();

        let maker_status = rx_maker_status.borrow().clone();
        yield maker_status.to_sse_event();

        yield Event::json(&identity).event("identity");

        let offers = rx_offers.borrow().clone();
        yield Event::json(&offers.long).event("long_offer");
        yield Event::json(&offers.short).event("short_offer");

        let quote = rx_quote.borrow().clone();
        yield quote.to_sse_event();

        let cfds = rx_cfds.borrow().clone();
        if let Some(cfds) = cfds {
            yield cfds.to_sse_event()
        }

        loop{
            select! {
                Ok(()) = rx_wallet.changed() => {
                    let wallet_info = rx_wallet.borrow().clone();
                    yield wallet_info.to_sse_event();
                },
                Ok(()) = rx_maker_status.changed() => {
                    let maker_status = rx_maker_status.borrow().clone();
                    yield maker_status.to_sse_event();
                },
                Ok(()) = rx_offers.changed() => {
                    let offers = rx_offers.borrow().clone();
                    yield Event::json(&offers.long).event("long_offer");
                    yield Event::json(&offers.short).event("short_offer");
                }
                Ok(()) = rx_cfds.changed() => {
                    let cfds = rx_cfds.borrow().clone();
                    if let Some(cfds) = cfds {
                        yield cfds.to_sse_event()
                    }
                }
                Ok(()) = rx_quote.changed() => {
                    let quote = rx_quote.borrow().clone();
                    yield quote.to_sse_event();
                }
                _ = heartbeat.tick() => {
                    yield Event::json(&Heartbeat::new()).event("heartbeat")
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Heartbeat {
    timestamp: Timestamp,
    interval: u64,
}

impl Heartbeat {
    pub fn new() -> Self {
        Self {
            timestamp: Timestamp::now(),
            interval: HEARTBEAT_INTERVAL_SECS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfdOrderRequest {
    pub order_id: OrderId,
    pub quantity: Usd,
    pub leverage: Leverage,
}

#[rocket::post("/cfd/order", data = "<cfd_order_request>")]
#[instrument(name = "POST /cfd/order", skip(taker, _auth), err)]
pub async fn post_order_request(
    cfd_order_request: Json<CfdOrderRequest>,
    taker: &State<Taker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    taker
        .place_order(
            cfd_order_request.order_id,
            cfd_order_request.quantity,
            cfd_order_request.leverage,
        )
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Order request failed")
                .detail(format!("{e:#}"))
        })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/<action>")]
#[instrument(name = "POST /cfd/<id>/<action>", skip(taker, _auth), err)]
pub async fn post_cfd_action(
    id: Uuid,
    action: String,
    taker: &State<Taker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    let id = OrderId::from(id);
    let action = action.parse().map_err(|_| {
        HttpApiProblem::new(StatusCode::BAD_REQUEST).detail(format!("Invalid action: {}", action))
    })?;

    let result = match action {
        CfdAction::AcceptOrder
        | CfdAction::RejectOrder
        | CfdAction::AcceptSettlement
        | CfdAction::RejectSettlement
        | CfdAction::AcceptRollover
        | CfdAction::RejectRollover => {
            return Err(HttpApiProblem::new(StatusCode::BAD_REQUEST)
                .detail(format!("taker cannot invoke action {action}")));
        }
        CfdAction::Commit => taker.commit(id).await,
        CfdAction::Settle => taker.propose_settlement(id).await,
    };

    result.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title(action.to_string() + " failed")
            .detail(format!("{e:#}"))
    })?;

    Ok(())
}

#[rocket::get("/alive")]
#[instrument(name = "GET /alive")]
pub fn get_health_check() {}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct MarginRequest {
    pub price: Price,
    pub quantity: Usd,
    pub leverage: Leverage,

    #[serde(with = "bdk::bitcoin::util::amount::serde::as_btc")]
    pub opening_fee: Amount,
}

/// Represents the collateral that has to be put up
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MarginResponse {
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_btc")]
    pub margin: Amount,

    /// Margin + fees
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_btc")]
    pub complete_initial_costs: Amount,
}

#[derive(RustEmbed)]
#[folder = "../taker-frontend/dist/taker"]
struct Asset;

#[rocket::get("/assets/<file..>")]
#[instrument(name = "GET /assets/<file>", skip_all)]
pub fn dist<'r>(file: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let filename = format!("assets/{}", file.display());
    Asset::get(&filename).into_response(file)
}

#[rocket::get("/<_paths..>", format = "text/html")]
#[instrument(name = "GET /<_paths>", skip_all)]
pub fn index<'r>(_paths: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let asset = Asset::get("index.html").ok_or(Status::NotFound)?;
    Ok::<(ContentType, Cow<[u8]>), Status>((ContentType::HTML, asset.data))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawRequest {
    address: bdk::bitcoin::Address,
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_btc")]
    amount: Amount,
    fee: f32,
}

#[rocket::post("/withdraw", data = "<withdraw_request>")]
#[instrument(name = "POST /withdraw", skip(taker, _auth), err)]
pub async fn post_withdraw_request(
    withdraw_request: Json<WithdrawRequest>,
    taker: &State<Taker>,
    network: &State<Network>,
    _auth: Authenticated,
) -> Result<String, HttpApiProblem> {
    let amount =
        (withdraw_request.amount != bdk::bitcoin::Amount::ZERO).then(|| withdraw_request.amount);

    let txid = taker
        .withdraw(
            amount,
            withdraw_request.address.clone(),
            bdk::FeeRate::from_sat_per_vb(withdraw_request.fee),
        )
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Could not proceed with withdraw request")
                .detail(format!("{e:#}"))
        })?;

    Ok(projection::to_mempool_url(txid, *network.inner()))
}

#[rocket::get("/metrics")]
#[instrument(name = "GET /metrics", skip_all, err)]
pub async fn get_metrics<'r>(_auth: Authenticated) -> Result<String, HttpApiProblem> {
    let metrics = prometheus::TextEncoder::new()
        .encode_to_string(&prometheus::gather())
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Failed to encode metrics")
                .detail(e.to_string())
        })?;

    Ok(metrics)
}

#[rocket::put("/sync")]
#[instrument(name = "PUT /sync", skip_all, err)]
pub async fn put_sync_wallet(
    taker: &State<Taker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    taker.sync_wallet().await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Could not sync wallet")
            .detail(format!("{e:#}"))
    })?;

    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    daemon_version: String,
}

#[rocket::get("/version")]
#[instrument(name = "GET /version")]
pub async fn get_version() -> Json<HealthCheck> {
    Json(HealthCheck {
        daemon_version: daemon::version::version().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_test::Token;

    #[test]
    fn heartbeat_serialization() {
        let heartbeat = Heartbeat {
            timestamp: Timestamp::new(0),
            interval: 1,
        };

        serde_test::assert_ser_tokens(
            &heartbeat,
            &[
                Token::Struct {
                    name: "Heartbeat",
                    len: 2,
                },
                Token::Str("timestamp"),
                Token::NewtypeStruct { name: "Timestamp" },
                Token::I64(0),
                Token::Str("interval"),
                Token::U64(1),
                Token::StructEnd,
            ],
        );
    }
}
