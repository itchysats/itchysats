use anyhow::Result;
use async_trait::async_trait;
pub use bitmex_stream::Network;
use futures::TryStreamExt;
use rust_decimal::Decimal;
use std::fmt;
use time::OffsetDateTime;
use tracing::Instrument;
use xtra_productivity::xtra_productivity;

pub const QUOTE_INTERVAL_MINUTES: i64 = 1;

pub struct Actor {
    latest_quote: Option<Quote>,

    /// Contains the reason we are stopping.
    stop_reason: Option<Error>,
    network: Network,
}

impl Actor {
    pub fn new(network: Network) -> Self {
        Self {
            latest_quote: None,
            stop_reason: None,
            network,
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = Error;

    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("we are alive");

        tokio_extras::spawn_fallible(
            &this.clone(),
            {
                let this = this.clone();
                let network = self.network;

                async move {
                    let mut stream = bitmex_stream::subscribe(
                        [format!("quoteBin{QUOTE_INTERVAL_MINUTES}m:XBTUSD")],
                        network,
                        None,
                    );

                    while let Some(text) = stream
                        .try_next()
                        .await
                        .map_err(|e| Error::Failed { source: e })?
                    {
                        let quote = Quote::from_str(&text)
                            .map_err(|e| Error::FailedToParseQuote { source: e })?;

                        match quote {
                            Some(quote) => {
                                let span = tracing::debug_span!(
                                    "Received new quote",
                                    bid = %quote.bid,
                                    ask = %quote.ask,
                                    timestamp = %quote.timestamp,
                                );

                                let is_our_address_disconnected = this
                                    .send(NewQuoteReceived(quote))
                                    .instrument(span)
                                    .await
                                    .is_err();

                                // Our task should already be dead and the actor restarted if this
                                // happens.
                                if is_our_address_disconnected {
                                    return Ok(());
                                }
                            }
                            None => {
                                continue;
                            }
                        }
                    }

                    Err(Error::StreamEnded)
                }
            },
            |err| async move {
                let _: Result<(), xtra::Error> = this.send(err).await;
            },
        );
    }

    async fn stopped(self) -> Self::Stop {
        self.stop_reason.unwrap_or(Error::Unspecified)
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle(&mut self, msg: Error, ctx: &mut xtra::Context<Self>) {
        self.stop_reason = Some(msg);
        ctx.stop_self();
    }

    async fn handle(&mut self, msg: NewQuoteReceived) {
        self.latest_quote = Some(msg.0);
    }

    async fn handle(&mut self, _: LatestQuote) -> Option<Quote> {
        self.latest_quote
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Connection to BitMex API failed")]
    Failed { source: bitmex_stream::Error },
    #[error("Websocket stream to BitMex API closed")]
    StreamEnded,
    #[error("Failed to parse quote")]
    FailedToParseQuote { source: anyhow::Error },
    #[error("Stop reason was not specified")]
    Unspecified,
}

/// Private message to update our internal state with the latest quote.
#[derive(Debug)]
struct NewQuoteReceived(Quote);

/// Request the latest quote from the price feed.
#[derive(Debug, Clone, Copy)]
pub struct LatestQuote;

#[derive(Clone, Copy)]
pub struct Quote {
    pub timestamp: OffsetDateTime,
    pub bid: Decimal,
    pub ask: Decimal,
}

impl fmt::Debug for Quote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rfc3339_timestamp = self
            .timestamp
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();

        f.debug_struct("Quote")
            .field("timestamp", &rfc3339_timestamp)
            .field("bid", &self.bid)
            .field("ask", &self.ask)
            .finish()
    }
}

impl Quote {
    fn from_str(text: &str) -> Result<Option<Self>> {
        let table_message = match serde_json::from_str::<wire::TableMessage>(text) {
            Ok(table_message) => table_message,
            Err(_) => {
                tracing::trace!(%text, "Not a 'table' message, skipping...");
                return Ok(None);
            }
        };

        let [quote] = table_message.data;

        Ok(Some(Self {
            timestamp: quote.timestamp,
            bid: quote.bid_price,
            ask: quote.ask_price,
        }))
    }

    pub fn bid(&self) -> Decimal {
        self.bid
    }

    pub fn ask(&self) -> Decimal {
        self.ask
    }

    pub fn is_older_than(&self, duration: time::Duration) -> bool {
        let required_quote_timestamp = (OffsetDateTime::now_utc() - duration).unix_timestamp();

        self.timestamp.unix_timestamp() < required_quote_timestamp
    }
}

mod wire {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    pub struct TableMessage {
        pub table: String,
        // we always just expect a single quote, hence the use of an array instead of a vec
        pub data: [QuoteData; 1],
    }

    #[derive(Debug, Clone, Deserialize, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct QuoteData {
        pub bid_size: u64,
        pub ask_size: u64,
        #[serde(with = "rust_decimal::serde::float")]
        pub bid_price: Decimal,
        #[serde(with = "rust_decimal::serde::float")]
        pub ask_price: Decimal,
        pub symbol: String,
        #[serde(with = "time::serde::rfc3339")]
        pub timestamp: OffsetDateTime,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use time::ext::NumericalDuration;

    #[test]
    fn can_deserialize_quote_message() {
        let quote = Quote::from_str(r#"{"table":"quoteBin1m","action":"insert","data":[{"timestamp":"2021-09-21T02:40:00.000Z","symbol":"XBTUSD","bidSize":50200,"bidPrice":42640.5,"askPrice":42641,"askSize":363600}]}"#).unwrap().unwrap();

        assert_eq!(quote.bid, dec!(42640.5));
        assert_eq!(quote.ask, dec!(42641));
        assert_eq!(quote.timestamp.unix_timestamp(), 1632192000)
    }

    #[test]
    fn quote_from_now_is_not_old() {
        let quote = dummy_quote_at(OffsetDateTime::now_utc());

        let is_older = quote.is_older_than(1.minutes());

        assert!(!is_older)
    }

    #[test]
    fn quote_from_one_hour_ago_is_old() {
        let quote = dummy_quote_at(OffsetDateTime::now_utc() - 1.hours());

        let is_older = quote.is_older_than(1.minutes());

        assert!(is_older)
    }

    fn dummy_quote_at(timestamp: OffsetDateTime) -> Quote {
        Quote {
            timestamp,
            bid: dec!(10),
            ask: dec!(10),
        }
    }
}
