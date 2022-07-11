use daemon::maia_core::secp256k1_zkp::SecretKey;
use daemon::maia_core::secp256k1_zkp::XOnlyPublicKey;
use daemon::oracle;
use model::olivia;
use model::olivia::BitMexPriceEventId;
use std::str::FromStr;
use time::ext::NumericalDuration;

#[allow(dead_code)]
#[derive(Clone)]
pub struct OliviaData {
    ids: Vec<BitMexPriceEventId>,
    pk: XOnlyPublicKey,
    nonce_pks: Vec<XOnlyPublicKey>,
    price: u64,
    attestations: Vec<SecretKey>,
}

impl OliviaData {
    pub fn example_0() -> Self {
        Self::example(
            Self::EVENT_ID_0,
            Self::PRICE_0,
            &Self::NONCE_PKS_0,
            &Self::ATTESTATIONS_0,
        )
    }

    pub fn example_1() -> Self {
        Self::example(
            Self::EVENT_ID_1,
            Self::PRICE_1,
            &Self::NONCE_PKS_1,
            &Self::ATTESTATIONS_1,
        )
    }

    /// Generate an example of all the data from `olivia` needed to test the
    /// CFD protocol end-to-end.
    fn example(id: &str, price: u64, nonce_pks: &[&str], attestations: &[&str]) -> Self {
        let oracle_pk = XOnlyPublicKey::from_str(Self::OLIVIA_PK).unwrap();

        let id = id.parse::<BitMexPriceEventId>().unwrap();
        let ids = olivia::hourly_events(id.timestamp(), id.timestamp() + 24.hours()).unwrap();

        let nonce_pks = nonce_pks
            .iter()
            .map(|pk| XOnlyPublicKey::from_str(pk).unwrap())
            .collect();

        let attestations = attestations
            .iter()
            .map(|pk| SecretKey::from_str(pk).unwrap())
            .collect();

        Self {
            ids,
            pk: oracle_pk,
            nonce_pks,
            attestations,
            price,
        }
    }

    pub fn announcements(&self) -> Vec<olivia::Announcement> {
        self.ids
            .clone()
            .into_iter()
            .map(|id| olivia::Announcement {
                id,
                expected_outcome_time: id.timestamp(),
                nonce_pks: self.nonce_pks.clone(),
            })
            .collect()
    }

    pub fn attestations(&self) -> Vec<oracle::Attestation> {
        self.ids
            .clone()
            .into_iter()
            .map(|id| {
                oracle::Attestation::new(olivia::Attestation {
                    id,
                    price: self.price,
                    scalars: self.attestations.clone(),
                })
            })
            .collect()
    }

    const OLIVIA_PK: &'static str =
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7";

    const EVENT_ID_0: &'static str = "/x/BitMEX/BXBT/2021-10-05T02:00:00.price?n=20";
    const NONCE_PKS_0: [&'static str; 20] = [
        "d02d163cf9623f567c4e3faf851a9266ac1ede13da4ca4141f3a7717fba9a739",
        "bc310f26aa5addbc382f653d8530aaead7c25e3546abc24639f490e36d4bdb88",
        "2661375f570dcc32300d442e85b6d72dfa3232dccda45e8fb4a2d1e758d1d374",
        "fcc68fbf071d391b14c0867cb4defb5a8abc12418dff3dfc2f84fd4025cb2716",
        "cf5c2b7fe3851c64a7ff9635a9bfc50cdd301401d002f2da049f4c6a20e8457b",
        "14f1005d8c2832a2c4666dd732dd9bb3af9c8f70ebcdaec96869b1ca0c8e0de6",
        "299ee1c9c20fab8b067adf452a7d7661b5e7f5dd6bc707562805002e7cb8443e",
        "bcb4e5a594346de298993a7a31762f598b5224b977e23182369e9ed3e5127f78",
        "25e09a16ee5d469069abfb62cd5e1f20af50cf15241f571e64fa28b127304574",
        "3ed5a1422f43299caf281123aba88bd4bc61ec863f4afb79c7ce7663ad44db5d",
        "a7e0f61212735c192c4bf16e0a3e925e65f9f3feb6f1e5e8d6f5c18cf2dbb5a8",
        "a36a631015d9036d0c321fea7cf12f589aa196e7279b4a290de5112c2940e540",
        "b5bdd931f81970139e7301ac654b378077c3ed993ca7893ed93fee5fc6f7a782",
        "00090816e256b41e042dce38bde99ab3cf9482f9b066836988d3ed54833638e8",
        "3530408e93c251f5f488d3b1c608157177c459d6fab1966abebf765bcc9338d2",
        "603269ce88d112ff7fcfcaab82f228be97deca37f8190084d509c71b51a30432",
        "f0587414fcc6c56aef11d4a1d287ad6b55b237c5b8a5d5d93eb9ca06f6466ccf",
        "763009afb0ffd99c7b835488cb3b0302f3b78f59bbfd5292bedab8ef9da8c1b7",
        "3867af9048309a05004a164bdea09899f23ff1d83b6491b2b53a1b7b92e0eb2e",
        "688118e6b59e27944c277513db2711a520f4283c7c53a11f58d9f6a46d82c964",
    ];
    const PRICE_0: u64 = 49262;
    const ATTESTATIONS_0: [&'static str; 20] = [
        "5bc7663195971daaa1e3e6a81b4bca65882791644bc446fc060cbc118a3ace0f",
        "721d0cb56a0778a1ca7907f81a0787f34385b13f854c845c4c5539f7f6267958",
        "044aeef0d525c8ff48758c80939e95807bc640990cc03f53ab6fc0b262045221",
        "79f5175423ec6ee69c8d0e55251db85f3015c2edfa5a03095443fbbf35eb2282",
        "233b9ec549e9cc7c702109d29636db85a3ec63a66f3b53444bcc7586d36ca439",
        "2961a00320b7c9a70220060019a6ca88e18c205fadd2f873c174e5ccbbed527e",
        "bdb76e8f81c39ade4205ead9b68118757fc49ec22769605f26ef904b235283d6",
        "6e75dafedf4ed685513ec1f5c93508de4fad2be05b46001ac00c03474f4690e1",
        "cfcfc27eb9273b343b3042f0386e77efe329066be079788bb00ab47d72f26780",
        "2d931ffd2963e74566365674583abc427bdb6ae571c4887d81f1920f0850665d",
        "33b6f1112fa046cbc04be44c615e70519702662c1f72d8d49b3c4613614a8a46",
        "19e569b15410fa9a758c1a6c211eae8c1547efbe0ac6a7709902be93415f2f09",
        "d859dd5c9a58e1836d1eea3ebe7f48198a681d29e5a5cd6922532d2e94a53a1d",
        "3387eb2ad5e64cd102167766bb72b447f4a2e5129d161e422f9d41cd7d1cc281",
        "db35a9778a1e3abc8d8ab2f4a79346ae2154c9e0b4932d859d1f3e244f67ae76",
        "c3be969e8b889cfb2ece71123e6be5538a2d3a1229637b18bccc179073c38059",
        "6f73263f430e10b82d0fd06c4ddd3b8a6b58c3e756745bd0d9e71a399e517921",
        "0818c9c245d7d2162cd393c562a121f80405a27d22ae465e95030c31ebb4bd24",
        "b7c03f0bd6d63bd78ad4ea0f3452ff9717ba65ca42038e6e90a1aa558b7942dc",
        "90c4d8ec9f408ccb62a62daa993c20f2f86799e1fdea520c6d060418e55fd216",
    ];

    const EVENT_ID_1: &'static str = "/x/BitMEX/BXBT/2021-10-05T08:00:00.price?n=20";
    const NONCE_PKS_1: [&'static str; 20] = [
        "150df2e64f39706e726eaa1fe081af3edf376d9644723e135a99328fd194caca",
        "b90629cedc7cb8430b4d15c84bbe1fe173e70e626d40c465e64de29d4879e20f",
        "ae14ffb8701d3e224b6632a1bb7b099c8aa90979c3fb788422daa08bca25fa68",
        "3717940a7e8c35b48b3596498ed93e4d54ba01a2bcbb645d30dae2fc98f087a8",
        "91beb5da91cc8b4ee6ae603e7ae41cc041d5ea2c13bae9f0e630c69f6c0adfad",
        "c51cafb450b01f30ec8bd2b4b5fed6f7e179f49945959f0d7609b4b9c5ab3781",
        "75f2d9332aa1b2d84446a4b2aa276b4c2853659ab0ba74f0881289d3ab700f0c",
        "5367de73acb53e69b0a4f777e564f87055fede5d4492ddafae876a815fa6166c",
        "2087a513adb1aa2cc8506ca58306723ed13ba82e054f5bf29fcbeef1ab915c5a",
        "71c980fb6adae9c121405628c91daffcc5ab52a8a0b6f53c953e8a0236b05782",
        "d370d22f06751fc649f6ee930ac7f8f3b00389fdad02883a8038a81c46c33b19",
        "fa6f7d37dc88b510c250dcae1023cce5009d5beb85a75f5b8b10c973b62348aa",
        "a658077f9c963d1f41cf63b7ebf6e08331f5d201554b3af7814673108abe1bf3",
        "8a816bf4caa2d6114b2e4d3ab9bff0d470ee0b90163c78c9b67f90238ead9319",
        "c2519a4e764a65204c469062e260d8565f7730847c507b92c987e478ca91abe1",
        "59cb6b5beac6511a671076530cc6cc9f1926f54c640828f38c363b110dd8a0cd",
        "4625b1f3ab9ee01455fa1a98d15fc8d73a7cf41becb4ca5c6eab88db0ba7c114",
        "82a4de403c604fe40aa3804c5ada6af54c425c0576980b50f259d32dc1a0fcff",
        "5c4fb87b3812982759ed7264676e713e4e477a41759261515b04797db393ef62",
        "f3f6b9134c0fdd670767fbf478fd0dd3430f195ce9c21cabb84f3c1dd4848a11",
    ];
    const PRICE_1: u64 = 49493;
    const ATTESTATIONS_1: [&'static str; 20] = [
        "605f458e9a7bd216ff522e45f6cd14378c03ccfd4d35a69b9b6ce5c4ebfc89fa",
        "edc7215277d2c24a7a4659ff8831352db609fcc467fead5e27fdada172cdfd86",
        "1c2d76fcbe724b1fabd2622b991e90bbb2ea9244489de960747134c9fd695dcb",
        "26b4f078c9ca2233b18b0e42c4bb9867e5de8ee35b500e30b28d9b1742322e49",
        "2b59aeaacb80056b45dc12d6525d5c75343ef75730623c8d9893e2b681bf4b85",
        "782e38e777d527e7cb0028a6d03e8f760c6202dbc5ac605f67f995919dee6182",
        "a902f37f71a78e4bcf431a778024bd775db6d7ade0626a9e7bc4cdf0b1e52dfd",
        "3927eb5ef3b56817c08709e0af1bb643ad4d95dbf5a92a49e1e9c8c811e929c4",
        "9ff44fa9d8377a3531792cd6362e4a5b24b86d85602749d301f8449859065b77",
        "6a2156ff0aaef174b36d5f8adc597fdcb26f306f7ef6e9a485faabc8eb29da2e",
        "53445b507c0de312959fe4566b82db93987dd0b854f1a33bbad7768512bcaf69",
        "793c40e0ec3a830c46658bfaed7df74e3fc6781e421e00db5b5f46b26ce4d092",
        "db7f800da2f22878c8fc8368047308146e1ebd6316c389303c07ebeed7488fc9",
        "73921d09e0d567a03f3a411c0f3455f9f652bbede808a694cca0fa94619f5ba9",
        "3d4bd70d93f20aa6b1621ccd077c90bcdee47ce2bae15155434a77a3153a3235",
        "90fc10577ab737e311b43288a266490f222a6ecb9f9667e01d7a54c0437d145f",
        "51d350616c6fdf90254240b757184fc0dd226328adb42be214ec25832854950e",
        "bab3a6269e172ac590fd36683724f087b4add293bb0ee4ef3d21fb5929985c75",
        "d65a4c71062fc0b0210bb3e239f60d826a37d28caadfc52edd7afde6e91ff818",
        "ea5dfd972784808a15543f850c7bc86bff2b51cff81ec68fc4c3977d5e7d38de",
    ];
}
