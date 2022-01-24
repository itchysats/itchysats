use bdk::bitcoin::Amount;
use bdk::bitcoin::Network;
use daemon::auth::Authenticated;
use daemon::bitmex_price_feed;
use daemon::connection::ConnectionStatus;
use daemon::model::cfd::OrderId;
use daemon::model::Leverage;
use daemon::model::Price;
use daemon::model::Usd;
use daemon::model::WalletInfo;
use daemon::oracle;
use daemon::projection;
use daemon::projection::CfdAction;
use daemon::projection::Feeds;
use daemon::routes::EmbeddedFileExt;
use daemon::to_sse_event::Heartbeat;
use daemon::to_sse_event::ToSseEvent;
use daemon::wallet;
use daemon::TakerActorSystem;
use http_api_problem::HttpApiProblem;
use http_api_problem::StatusCode;
use rocket::http::ContentType;
use rocket::http::Status;
use rocket::response::stream::EventStream;
use rocket::response::Responder;
use rocket::serde::json::Json;
use rocket::State;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde::Serialize;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::select;
use tokio::sync::watch;
use uuid::Uuid;

type Taker = TakerActorSystem<oracle::Actor, wallet::Actor, bitmex_price_feed::Actor>;

#[rocket::get("/feed")]
pub async fn feed(
    rx: &State<Feeds>,
    rx_wallet: &State<watch::Receiver<Option<WalletInfo>>>,
    rx_maker_status: &State<watch::Receiver<ConnectionStatus>>,
    _auth: Authenticated,
) -> EventStream![] {
    const HEARTBEAT_INTERVAL_SECS: u64 = 5;

    let rx = rx.inner();
    let mut rx_cfds = rx.cfds.clone();
    let mut rx_order = rx.order.clone();
    let mut rx_quote = rx.quote.clone();
    let mut rx_wallet = rx_wallet.inner().clone();
    let mut rx_maker_status = rx_maker_status.inner().clone();
    let mut heartbeat =
        tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));

    EventStream! {
        let wallet_info = rx_wallet.borrow().clone();
        yield wallet_info.to_sse_event();

        let maker_status = rx_maker_status.borrow().clone();
        yield maker_status.to_sse_event();

        let order = rx_order.borrow().clone();
        yield order.to_sse_event();

        let quote = rx_quote.borrow().clone();
        yield quote.to_sse_event();

        let cfds = rx_cfds.borrow().clone();
        yield cfds.to_sse_event();

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
                Ok(()) = rx_order.changed() => {
                    let order = rx_order.borrow().clone();
                    yield order.to_sse_event();
                }
                Ok(()) = rx_cfds.changed() => {
                    let cfds = rx_cfds.borrow().clone();
                    yield cfds.to_sse_event();
                }
                Ok(()) = rx_quote.changed() => {
                    let quote = rx_quote.borrow().clone();
                    yield quote.to_sse_event();
                }
                _ = heartbeat.tick() => {
                    yield Heartbeat::new(HEARTBEAT_INTERVAL_SECS).to_sse_event();
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CfdOrderRequest {
    pub order_id: OrderId,
    pub quantity: Usd,
}

#[rocket::post("/cfd/order", data = "<cfd_order_request>")]
pub async fn post_order_request(
    cfd_order_request: Json<CfdOrderRequest>,
    taker: &State<Taker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    taker
        .take_offer(cfd_order_request.order_id, cfd_order_request.quantity)
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Order request failed")
                .detail(format!("{e:#}"))
        })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/<action>")]
pub async fn post_cfd_action(
    id: Uuid,
    action: CfdAction,
    taker: &State<Taker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    let id = OrderId::from(id);

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
pub fn get_health_check() {}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct MarginRequest {
    pub price: Price,
    pub quantity: Usd,
    pub leverage: Leverage,

    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    pub opening_fee: Amount,
}

/// Represents the collateral that has to be put up
#[derive(Debug, Clone, Copy, Serialize)]
pub struct MarginResponse {
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    pub margin: Amount,

    /// Margin + fees
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    pub complete_initial_costs: Amount,
}

#[derive(RustEmbed)]
#[folder = "../taker-frontend/dist/taker"]
struct Asset;

#[rocket::get("/assets/<file..>")]
pub fn dist<'r>(file: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let filename = format!("assets/{}", file.display());
    Asset::get(&filename).into_response(file)
}

#[rocket::get("/<_paths..>", format = "text/html")]
pub fn index<'r>(_paths: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let asset = Asset::get("index.html").ok_or(Status::NotFound)?;
    Ok::<(ContentType, Cow<[u8]>), Status>((ContentType::HTML, asset.data))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawRequest {
    address: bdk::bitcoin::Address,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    amount: Amount,
    fee: f32,
}

#[rocket::post("/withdraw", data = "<withdraw_request>")]
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
