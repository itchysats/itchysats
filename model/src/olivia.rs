use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::XOnlyPublicKey;
use conquer_once::Lazy;
use maia_core::secp256k1_zkp::SecretKey;
use serde::Deserialize;
use serde_with::DeserializeFromStr;
use serde_with::SerializeDisplay;
use std::fmt;
use std::str;
use std::str::FromStr;
use time::ext::NumericalDuration;
use time::format_description::FormatItem;
use time::macros::format_description;
use time::Duration;
use time::OffsetDateTime;
use time::PrimitiveDateTime;
use time::Time;
use url::Url;

pub const EVENT_TIME_FORMAT: &[FormatItem] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");

pub static PUBLIC_KEY: Lazy<XOnlyPublicKey> = Lazy::new(|| {
    XOnlyPublicKey::from_str("ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7")
        .expect("static key to be valid")
});

#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
#[serde(try_from = "olivia_api::Response")]
pub struct Announcement {
    /// Identifier for an oracle event.
    ///
    /// Doubles up as the path of the URL for this event i.e.
    /// <https://h00.ooo/>{id}.
    pub id: BitMexPriceEventId,
    pub expected_outcome_time: OffsetDateTime,
    pub nonce_pks: Vec<XOnlyPublicKey>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(try_from = "olivia_api::Response")]
pub struct Attestation {
    pub id: BitMexPriceEventId,
    pub price: u64,
    pub scalars: Vec<SecretKey>,
}

#[derive(
    Debug, Clone, Copy, SerializeDisplay, DeserializeFromStr, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct BitMexPriceEventId {
    /// The timestamp this price event refers to.
    timestamp: OffsetDateTime,
    digits: usize,
}

impl BitMexPriceEventId {
    pub fn new(timestamp: OffsetDateTime, digits: usize) -> Self {
        let (hours, minutes, seconds) = timestamp.time().as_hms();
        let time_without_nanos =
            Time::from_hms(hours, minutes, seconds).expect("original timestamp was valid");

        let timestamp_without_nanos = timestamp.replace_time(time_without_nanos);

        Self {
            timestamp: timestamp_without_nanos,
            digits,
        }
    }

    pub fn with_20_digits(timestamp: OffsetDateTime) -> Self {
        Self::new(timestamp, 20)
    }

    /// Checks whether this event has likely already occurred.
    ///
    /// We can't be sure about it because our local clock might be off from the oracle's clock.
    pub fn has_likely_occurred(&self) -> bool {
        let now = OffsetDateTime::now_utc();

        now > self.timestamp + Duration::minutes(1)
    }

    pub fn to_olivia_url(self) -> Url {
        "https://h00.ooo"
            .parse::<Url>()
            .expect("valid URL from constant")
            .join(&self.to_string())
            .expect("Event id can be joined")
    }

    pub fn timestamp(&self) -> OffsetDateTime {
        self.timestamp
    }

    pub fn digits(&self) -> usize {
        self.digits
    }
}

impl fmt::Display for BitMexPriceEventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "/x/BitMEX/BXBT/{}.price?n={}",
            self.timestamp
                .format(&EVENT_TIME_FORMAT)
                .expect("should always format and we can't return an error here"),
            self.digits
        )
    }
}

impl FromStr for BitMexPriceEventId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let remaining = s.trim_start_matches("/x/BitMEX/BXBT/");
        let (timestamp, rest) = remaining.split_at(19);
        let digits = rest.trim_start_matches(".price?n=");

        Ok(Self {
            timestamp: PrimitiveDateTime::parse(timestamp, &EVENT_TIME_FORMAT)
                .with_context(|| format!("Failed to parse {timestamp} as timestamp"))?
                .assume_utc(),
            digits: digits.parse()?,
        })
    }
}

impl From<Announcement> for maia_core::Announcement {
    fn from(announcement: Announcement) -> Self {
        maia_core::Announcement {
            id: announcement.id.to_string(),
            nonce_pks: announcement.nonce_pks,
        }
    }
}

/// Produce a list of hourly events ranging from `start` to `end`, to
/// the _next_ hour.
pub fn hourly_events(
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Vec<BitMexPriceEventId>> {
    let start_adjusted = ceil_to_next_hour(start);
    let end_adjusted = ceil_to_next_hour(end);
    let announcements = spaced_events(start_adjusted, end_adjusted, Duration::HOUR)?;

    Ok(announcements)
}

/// Produce an inclusive range of events going from `start` to `end`.
/// The space between each event is defined by the argument
/// `interval`.
pub fn spaced_events(
    start: OffsetDateTime,
    end: OffsetDateTime,
    interval: Duration,
) -> Result<Vec<BitMexPriceEventId>> {
    ensure!(end > start, "end must be later than start");

    Ok((start.unix_timestamp()..=end.unix_timestamp())
        .step_by(interval.whole_seconds() as usize)
        .map(OffsetDateTime::from_unix_timestamp)
        .map(Result::unwrap) // roundtrip should work
        .map(BitMexPriceEventId::with_20_digits)
        .collect())
}

pub fn next_announcement_after(timestamp: OffsetDateTime) -> BitMexPriceEventId {
    let adjusted = ceil_to_next_hour(timestamp);

    BitMexPriceEventId::with_20_digits(adjusted)
}

fn ceil_to_next_hour(original: OffsetDateTime) -> OffsetDateTime {
    let timestamp = original + 1.hours();
    let exact_hour = Time::from_hms(timestamp.hour(), 0, 0).expect(
        "Exact hour from timestamp to be always within range, both docs and tests confirm it",
    );
    timestamp.replace_time(exact_hour)
}

mod olivia_api {
    use super::*;
    use anyhow::Context;
    use std::convert::TryFrom;
    use time::OffsetDateTime;

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Response {
        announcement: Announcement,
        attestation: Option<Attestation>,
    }

    impl TryFrom<Response> for super::Announcement {
        type Error = serde_json::Error;

        fn try_from(response: Response) -> Result<Self, Self::Error> {
            // TODO: Verify signature here

            let data =
                serde_json::from_str::<AnnouncementData>(&response.announcement.oracle_event.data)?;

            Ok(Self {
                id: data.id,
                expected_outcome_time: data.expected_outcome_time,
                nonce_pks: data.schemes.olivia_v1.nonces,
            })
        }
    }

    impl TryFrom<Response> for super::Attestation {
        type Error = anyhow::Error;

        fn try_from(response: Response) -> Result<Self, Self::Error> {
            // TODO: Verify signature here

            let data =
                serde_json::from_str::<AnnouncementData>(&response.announcement.oracle_event.data)?;
            let attestation = response.attestation.context("attestation missing")?;

            Ok(Self {
                id: data.id,
                price: attestation.outcome.parse()?,
                scalars: attestation.schemes.olivia_v1.scalars,
            })
        }
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Announcement {
        oracle_event: OracleEvent,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct OracleEvent {
        data: String,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    #[serde(rename_all = "kebab-case")]
    struct AnnouncementData {
        id: BitMexPriceEventId,
        #[serde(with = "timestamp")]
        expected_outcome_time: OffsetDateTime,
        schemes: Schemes,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct Attestation {
        outcome: String,
        schemes: Schemes,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct Schemes {
        #[serde(rename = "olivia-v1")]
        olivia_v1: OliviaV1,
    }

    #[derive(Debug, Clone, serde::Deserialize)]
    struct OliviaV1 {
        #[serde(default)]
        scalars: Vec<SecretKey>,
        #[serde(default)]
        nonces: Vec<XOnlyPublicKey>,
    }

    mod timestamp {
        use crate::olivia;
        use serde::de::Error as _;
        use serde::Deserialize;
        use serde::Deserializer;
        use time::OffsetDateTime;
        use time::PrimitiveDateTime;

        pub fn deserialize<'a, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
        where
            D: Deserializer<'a>,
        {
            let string = String::deserialize(deserializer)?;
            let date_time = PrimitiveDateTime::parse(&string, &olivia::EVENT_TIME_FORMAT)
                .map_err(D::Error::custom)?;

            Ok(date_time.assume_utc())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::vec;

        use crate::olivia;
        use crate::olivia::BitMexPriceEventId;
        use time::macros::datetime;

        #[test]
        fn deserialize_announcement() {
            let json = r#"{"announcement":{"oracle_event":{"encoding":"json","data":"{\"id\":\"/x/BitMEX/BXBT/2021-10-04T22:00:00.price?n=20\",\"expected-outcome-time\":\"2021-10-04T22:00:00\",\"descriptor\":{\"type\":\"digit-decomposition\",\"is_signed\":false,\"n_digits\":20,\"unit\":null},\"schemes\":{\"olivia-v1\":{\"nonces\":[\"8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239\",\"77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e\",\"e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89\",\"ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a\",\"52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4\",\"e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40\",\"612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e\",\"e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90\",\"281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197\",\"3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033\",\"29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9\",\"20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523\",\"667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f\",\"224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1\",\"85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4\",\"96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b\",\"9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493\",\"b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6\",\"bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde\",\"20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe\"]},\"ecdsa-v1\":{}}}"},"signature":"743ed9900aba5a1ba3ba9d862628cdc5cca27974c40c4ab64618709021b3fbb13216a3efc733be260025da487ae9b63a8290d555bdc8da6324deff149fc7b110"},"attestation":{"outcome":"48935","schemes":{"olivia-v1":{"scalars":["1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484","72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6","051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e","847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7","5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998","368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee","9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999","fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738","44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086","587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d","010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955","5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520","06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd","fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe","0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068","c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f","86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269","fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db","41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d","ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"]},"ecdsa-v1":{"signature":"1d9a5e2336883cc6b440ff40e16ee44f8af2ba9313e46f1e4cd417f7dba7686279b0216e4b0b5fcf0c650dbad98fdefcf5ef16b49d63651a87f80caddd472384"}},"time":"2021-10-04T22:00:15"}}"#;

            let deserialized = serde_json::from_str::<olivia::Announcement>(json).unwrap();
            let expected = olivia::Announcement {
                id: BitMexPriceEventId::with_20_digits(datetime!(2021-10-04 22:00:00).assume_utc()),
                expected_outcome_time: datetime!(2021-10-04 22:00:00).assume_utc(),
                nonce_pks: vec![
                    "8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239"
                        .parse()
                        .unwrap(),
                    "77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e"
                        .parse()
                        .unwrap(),
                    "e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89"
                        .parse()
                        .unwrap(),
                    "ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a"
                        .parse()
                        .unwrap(),
                    "52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4"
                        .parse()
                        .unwrap(),
                    "e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40"
                        .parse()
                        .unwrap(),
                    "612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e"
                        .parse()
                        .unwrap(),
                    "e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90"
                        .parse()
                        .unwrap(),
                    "281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197"
                        .parse()
                        .unwrap(),
                    "3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033"
                        .parse()
                        .unwrap(),
                    "29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9"
                        .parse()
                        .unwrap(),
                    "20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523"
                        .parse()
                        .unwrap(),
                    "667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f"
                        .parse()
                        .unwrap(),
                    "224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1"
                        .parse()
                        .unwrap(),
                    "85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4"
                        .parse()
                        .unwrap(),
                    "96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b"
                        .parse()
                        .unwrap(),
                    "9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493"
                        .parse()
                        .unwrap(),
                    "b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6"
                        .parse()
                        .unwrap(),
                    "bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde"
                        .parse()
                        .unwrap(),
                    "20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe"
                        .parse()
                        .unwrap(),
                ],
            };

            assert_eq!(deserialized, expected)
        }

        #[test]
        fn deserialize_attestation() {
            let json = r#"{"announcement":{"oracle_event":{"encoding":"json","data":"{\"id\":\"/x/BitMEX/BXBT/2021-10-04T22:00:00.price?n=20\",\"expected-outcome-time\":\"2021-10-04T22:00:00\",\"descriptor\":{\"type\":\"digit-decomposition\",\"is_signed\":false,\"n_digits\":20,\"unit\":null},\"schemes\":{\"olivia-v1\":{\"nonces\":[\"8d72028eeaf4b85aec0f750f05a4a320cac193f5d8494bfe05cd4b29f3df4239\",\"77240f79a0042adae35ad24284b18b906f17a979fcec3c90d11ed682c6b9261e\",\"e42332407b58f7c6e860b886acfe8d19636fb21a1e20722522206b30a2424d89\",\"ce1158e02dc265751887edae9bdcf8d06ad40489c7643324ccb6a46e4e740f5a\",\"52a5751a43046217bcf009df917c24e400c6da645474a654a5f89499df7154d4\",\"e7b97360a952c2b239d1bfeaade73da4a38e83d20f5deb5b054bcbbc78c91e40\",\"612ce13fd61be10e8de77976c6d479865bc3d2ebdc212946f1e5d93e3f504d2e\",\"e40decd0ea27003b873dde9b6be02f1b344e7e74bc5299144fa0f37b1cf12e90\",\"281a829e05d5f8b96eaf620c7b26115bfb29013d503b6bb40068cdb413a87197\",\"3c87eed0a3852953b0f3ac8a47ff194de66c7229c42e6578e0f6464ba240f033\",\"29028525277cb39adab9ac145d6ce61f2e10306e7b6ce95970a22ea3b201a5d9\",\"20971b4d2069d8b9b5c5678290ab7624821cf32ffe32a20d58428ca90da02523\",\"667a9af33ed45bfb5c4fc7adacea15bbe26df90e0df7dd5b8235e14dfd0da38f\",\"224df2d2706b5c629173b84927e2b206dad7a72e132eb86912d9464dad4b41d1\",\"85296962b9d1f7699c248467ce94ce4aa6e00d26fe01af3a507bcd3a303855d4\",\"96813c9f4d136f0f64be79e73d657fecc43d8b6c463163913b4fa31f96b1ae6b\",\"9d5971aa596923560b12f367fb2f4e192d8906bf6ed3a58b093f50d3cad27493\",\"b7f2c135db80cee02b4436557c78dc1dd2343c1a3688ba736c6c40e9531547b6\",\"bd6236fc18f1dc96f9755cc5c435adaf3952ff810d3ad5b96a03464a61eecfde\",\"20b2922ce326e5e2f4ed683723a879e467edd1068bf5a3c4f331525216227abe\"]},\"ecdsa-v1\":{}}}"},"signature":"743ed9900aba5a1ba3ba9d862628cdc5cca27974c40c4ab64618709021b3fbb13216a3efc733be260025da487ae9b63a8290d555bdc8da6324deff149fc7b110"},"attestation":{"outcome":"48935","schemes":{"olivia-v1":{"scalars":["1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484","72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6","051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e","847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7","5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998","368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee","9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999","fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738","44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086","587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d","010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955","5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520","06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd","fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe","0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068","c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f","86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269","fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db","41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d","ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"]},"ecdsa-v1":{"signature":"1d9a5e2336883cc6b440ff40e16ee44f8af2ba9313e46f1e4cd417f7dba7686279b0216e4b0b5fcf0c650dbad98fdefcf5ef16b49d63651a87f80caddd472384"}},"time":"2021-10-04T22:00:15"}}"#;

            let deserialized = serde_json::from_str::<olivia::Attestation>(json).unwrap();
            let expected = olivia::Attestation {
                id: BitMexPriceEventId::with_20_digits(datetime!(2021-10-04 22:00:00).assume_utc()),
                price: 48935,
                scalars: vec![
                    "1327b3bd0f1faf45d6fed6c96d0c158da22a2033a6fed98bed036df0a4eef484"
                        .parse()
                        .unwrap(),
                    "72659c6beebd45e299bc4260a1c1ffd708ed33771459563502f25fc4f537cef6"
                        .parse()
                        .unwrap(),
                    "051eec45417e2493f36b13f4fdf83fb981be42901bf876e4ac594ff2daa4c30e"
                        .parse()
                        .unwrap(),
                    "847d8c7204335b1dbc2078cfb56118b1977162e7b997f2029f490929bbd603c7"
                        .parse()
                        .unwrap(),
                    "5b695846292b6d69d9beedcc7dd2b7e49fd49ec4fcf262d9357f52b049fa8998"
                        .parse()
                        .unwrap(),
                    "368a1f2206fcedcde37381b272fa5a400f55ef720ee2b8fff558e3b0dce729ee"
                        .parse()
                        .unwrap(),
                    "9e1c015c0e827037f18681937764f4973ef22d6fbbd82f6bde3bf5198f6b8999"
                        .parse()
                        .unwrap(),
                    "fe9620c9ad9862b5615f8cf3e20e8d9f422e7410914ce8af2b8bad8937b75738"
                        .parse()
                        .unwrap(),
                    "44297ae831898f8f5c7e57720f233a717e9034a5b41d6c89cce6d9058c4ee086"
                        .parse()
                        .unwrap(),
                    "587fc9b71f1920df825138f00bc625e6610e61b1fec0a64e2800fc05b3a2e96d"
                        .parse()
                        .unwrap(),
                    "010377f6b885ae48d62e7863c8038240aafe0a7fb97d58ac6173186c95335955"
                        .parse()
                        .unwrap(),
                    "5243782226739f59b0ac01a56a63537289ffe81b87b33eca42f89f7848623520"
                        .parse()
                        .unwrap(),
                    "06184cb8e46b5d520cd9b5829feeb73b688d61e5f37b91ff88d3f9b8664a5cdd"
                        .parse()
                        .unwrap(),
                    "fe48f4b568bb501732c4e8f1919940c9bca0ad909f4624658b14664af823ccfe"
                        .parse()
                        .unwrap(),
                    "0841f121e7a54f88a844227cd0ae62171b49d004120c16d1a1d619f0b76f7068"
                        .parse()
                        .unwrap(),
                    "c4ac3c8751a63f7c40062b9b84f2bb953b0e6bd8f2cf3b2bcaf711321e92df8f"
                        .parse()
                        .unwrap(),
                    "86a2b1a31bf80f17c00ab28420c636c1ed604d0b1f0a33adda99a0cf1e510269"
                        .parse()
                        .unwrap(),
                    "fb892eba992b723a06bccad6a2a1bb875d548a275a987266fceed097b9fd88db"
                        .parse()
                        .unwrap(),
                    "41991fb15fdb013ccab3e6674b91546a0e1e56a1e212c8795c76d0b43f4c884d"
                        .parse()
                        .unwrap(),
                    "ab6a4368d2e5e7cea23fd648662769facc1c37f1d1613225e9010af07cd74711"
                        .parse()
                        .unwrap(),
                ],
            };

            assert_eq!(deserialized, expected)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;
    use time::macros::datetime;

    #[test]
    fn to_olivia_url() {
        let url = BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc())
            .to_olivia_url();

        assert_eq!(
            url,
            "https://h00.ooo/x/BitMEX/BXBT/2021-09-23T10:00:00.price?n=20"
                .parse()
                .unwrap()
        );
    }

    #[test]
    fn parse_event_id() {
        let parsed = "/x/BitMEX/BXBT/2021-09-23T10:00:00.price?n=20"
            .parse::<BitMexPriceEventId>()
            .unwrap();
        let expected =
            BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc());

        assert_eq!(parsed, expected);
    }

    #[test]
    fn new_event_has_no_nanos() {
        let now = BitMexPriceEventId::with_20_digits(OffsetDateTime::now_utc());

        assert_eq!(now.timestamp.nanosecond(), 0);
    }

    #[test]
    fn has_occured_if_in_the_past() {
        let past_event =
            BitMexPriceEventId::with_20_digits(datetime!(2021-09-23 10:00:00).assume_utc());

        assert!(past_event.has_likely_occurred());
    }

    #[test]
    fn next_event_id_after_timestamp() {
        let event_id = next_announcement_after(datetime!(2021-09-23 10:40:00).assume_utc());

        assert_eq!(
            event_id.to_string(),
            "/x/BitMEX/BXBT/2021-09-23T11:00:00.price?n=20"
        );
    }

    #[test]
    fn next_event_id_is_midnight_next_day() {
        let event_id = next_announcement_after(datetime!(2021-09-23 23:40:00).assume_utc());

        assert_eq!(
            event_id.to_string(),
            "/x/BitMEX/BXBT/2021-09-24T00:00:00.price?n=20"
        );
    }

    #[test]
    fn range_of_24_hourly_events() {
        let actual = hourly_events(
            datetime!(2022-07-05 23:40:00).assume_utc(),
            datetime!(2022-07-06 23:40:00).assume_utc(),
        )
        .unwrap()
        .iter()
        .map(|event| event.timestamp)
        .collect_vec();

        let expected = vec![
            datetime!(2022-07-06 00:00:00).assume_utc(),
            datetime!(2022-07-06 01:00:00).assume_utc(),
            datetime!(2022-07-06 02:00:00).assume_utc(),
            datetime!(2022-07-06 03:00:00).assume_utc(),
            datetime!(2022-07-06 04:00:00).assume_utc(),
            datetime!(2022-07-06 05:00:00).assume_utc(),
            datetime!(2022-07-06 06:00:00).assume_utc(),
            datetime!(2022-07-06 07:00:00).assume_utc(),
            datetime!(2022-07-06 08:00:00).assume_utc(),
            datetime!(2022-07-06 09:00:00).assume_utc(),
            datetime!(2022-07-06 10:00:00).assume_utc(),
            datetime!(2022-07-06 11:00:00).assume_utc(),
            datetime!(2022-07-06 12:00:00).assume_utc(),
            datetime!(2022-07-06 13:00:00).assume_utc(),
            datetime!(2022-07-06 14:00:00).assume_utc(),
            datetime!(2022-07-06 15:00:00).assume_utc(),
            datetime!(2022-07-06 16:00:00).assume_utc(),
            datetime!(2022-07-06 17:00:00).assume_utc(),
            datetime!(2022-07-06 18:00:00).assume_utc(),
            datetime!(2022-07-06 19:00:00).assume_utc(),
            datetime!(2022-07-06 20:00:00).assume_utc(),
            datetime!(2022-07-06 21:00:00).assume_utc(),
            datetime!(2022-07-06 22:00:00).assume_utc(),
            datetime!(2022-07-06 23:00:00).assume_utc(),
            datetime!(2022-07-07 00:00:00).assume_utc(),
        ];

        assert_eq!(expected, actual);
    }

    #[test]
    fn range_of_30_events_per_minute() {
        let actual = spaced_events(
            datetime!(2022-07-05 00:00:00).assume_utc(),
            datetime!(2022-07-05 00:30:00).assume_utc(),
            Duration::MINUTE,
        )
        .unwrap()
        .iter()
        .map(|event| event.timestamp)
        .collect_vec();

        let expected = vec![
            datetime!(2022-07-05 00:00:00).assume_utc(),
            datetime!(2022-07-05 00:01:00).assume_utc(),
            datetime!(2022-07-05 00:02:00).assume_utc(),
            datetime!(2022-07-05 00:03:00).assume_utc(),
            datetime!(2022-07-05 00:04:00).assume_utc(),
            datetime!(2022-07-05 00:05:00).assume_utc(),
            datetime!(2022-07-05 00:06:00).assume_utc(),
            datetime!(2022-07-05 00:07:00).assume_utc(),
            datetime!(2022-07-05 00:08:00).assume_utc(),
            datetime!(2022-07-05 00:09:00).assume_utc(),
            datetime!(2022-07-05 00:10:00).assume_utc(),
            datetime!(2022-07-05 00:11:00).assume_utc(),
            datetime!(2022-07-05 00:12:00).assume_utc(),
            datetime!(2022-07-05 00:13:00).assume_utc(),
            datetime!(2022-07-05 00:14:00).assume_utc(),
            datetime!(2022-07-05 00:15:00).assume_utc(),
            datetime!(2022-07-05 00:16:00).assume_utc(),
            datetime!(2022-07-05 00:17:00).assume_utc(),
            datetime!(2022-07-05 00:18:00).assume_utc(),
            datetime!(2022-07-05 00:19:00).assume_utc(),
            datetime!(2022-07-05 00:20:00).assume_utc(),
            datetime!(2022-07-05 00:21:00).assume_utc(),
            datetime!(2022-07-05 00:22:00).assume_utc(),
            datetime!(2022-07-05 00:23:00).assume_utc(),
            datetime!(2022-07-05 00:24:00).assume_utc(),
            datetime!(2022-07-05 00:25:00).assume_utc(),
            datetime!(2022-07-05 00:26:00).assume_utc(),
            datetime!(2022-07-05 00:27:00).assume_utc(),
            datetime!(2022-07-05 00:28:00).assume_utc(),
            datetime!(2022-07-05 00:29:00).assume_utc(),
            datetime!(2022-07-05 00:30:00).assume_utc(),
        ];

        assert_eq!(expected, actual);
    }
}
