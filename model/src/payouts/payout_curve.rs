use crate::CompleteFee;
use crate::Leverage;
use crate::Price;
use crate::Usd;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin;
use curve::Curve;
use ndarray::prelude::*;
use num::FromPrimitive;
use num::ToPrimitive;
use rust_decimal::Decimal;
use std::fmt;
use std::ops::RangeInclusive;

mod basis;
mod basis_eval;
mod compat;
mod csr_tools;
mod curve;
mod curve_factory;
mod splineobject;
mod utils;

/// Generate a list of [`Payout`]s.
///
/// A key item to note is that although the POC logic has been to imposed
/// that maker goes short every time, there is no reason to make the math
/// have this imposition as well.
///
/// As well, the POC has also demanded that the Maker always has unity
/// leverage, hence why the ability to to specify this amount has been
/// omitted from the parameters. Internally, it is hard-coded to unity
/// in the call to PayoutCurve::new(), so this behaviour can be changed in
/// the future trivially.
///
/// ### Parameters
///
/// * price: BTC-USD exchange rate used to create CFD contract
/// * quantity: Interger number of one-dollar USD contracts contained in the
/// CFD; expressed as a Usd amount
/// * long_leverage: leverage used by the party with the long position
/// * short_leverage: leverage used by the party with the short position
/// * n_payouts: number of segments into which the payout curve is discretised
/// * fee: offset applied to the curve representing a fee paid between parties
///
/// ### Returns
///
/// The list of [`Payout`]s for the given price, quantity and leverage.
pub fn calculate(
    price: Price,
    quantity: Usd,
    long_leverage: Leverage,
    short_leverage: Leverage,
    n_payouts: usize,
    fee: CompleteFee,
) -> Result<Vec<Payout>> {
    let payouts = calculate_payout_parameters(
        price,
        quantity,
        long_leverage,
        short_leverage,
        n_payouts,
        fee,
    )?
    .into_iter()
    .map(PayoutParameter::into_payout)
    .collect::<Vec<_>>();

    Ok(payouts)
}

pub struct Payout {
    pub long: bitcoin::Amount,
    pub short: bitcoin::Amount,
    pub range: RangeInclusive<u64>,
}

const CONTRACT_VALUE: f64 = 1.;

/// Internal calculate function for the payout curve.
///
/// To ease testing, we write our tests against this function because it has a more human-friendly
/// output. The design goal here is that the the above `calculate` function is as thin as possible.
fn calculate_payout_parameters(
    price: Price,
    quantity: Usd,
    long_leverage: Leverage,
    short_leverage: Leverage,
    n_payouts: usize,
    fee: CompleteFee,
) -> Result<Vec<PayoutParameter>> {
    let initial_rate = price
        .try_into_f64()
        .context("Cannot convert price to f64")?;
    let quantity = quantity
        .try_into_u64()
        .context("Cannot convert quantity to u64")? as usize;

    let payout_curve = PayoutCurve::new(
        initial_rate,
        long_leverage.get() as usize,
        short_leverage.get() as usize,
        quantity,
        CONTRACT_VALUE,
        None,
    )?;

    let payout_parameters = payout_curve
        .generate_payout_scheme(n_payouts)?
        .rows()
        .into_iter()
        .map(|row| {
            let left_bound = row[0] as u64;
            let right_bound = row[1] as u64;
            let long_amount_btc = row[2];

            let long_amount = to_sats(long_amount_btc)?;
            let short_amount = to_sats(payout_curve.total_value - long_amount_btc)?;

            // We use `saturating_sub` when deducting fees because the
            // adjusted payout cannot go below zero. If the original
            // payout is close or equal to zero and the fee is
            // sufficiently large we would overflow otherwise.
            let (short_amount, long_amount) = match fee {
                CompleteFee::LongPaysShort(fee) => {
                    let long_minus_fee = long_amount.saturating_sub(fee.as_sat());
                    let long_fee_deduction = (long_amount as i64) - (long_minus_fee as i64);
                    let short_plus_fee = (short_amount as i64) + long_fee_deduction;

                    (short_plus_fee as u64, long_minus_fee as u64)
                }
                CompleteFee::ShortPaysLong(fee) => {
                    let short_minus_fee = short_amount.saturating_sub(fee.as_sat());
                    let short_fee_deduction = (short_amount as i64) - (short_minus_fee as i64);
                    let long_plus_fee = (long_amount as i64) + short_fee_deduction;

                    (short_minus_fee as u64, long_plus_fee as u64)
                }
                CompleteFee::None => (short_amount, long_amount),
            };

            Ok(PayoutParameter {
                left_bound,
                right_bound,
                long_amount,
                short_amount,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(payout_parameters)
}

#[derive(PartialEq, Clone, Copy)]
struct PayoutParameter {
    left_bound: u64,
    right_bound: u64,
    long_amount: u64,
    short_amount: u64,
}

impl PayoutParameter {
    fn into_payout(self) -> Payout {
        Payout {
            long: bitcoin::Amount::from_sat(self.long_amount),
            short: bitcoin::Amount::from_sat(self.short_amount),
            range: self.left_bound..=self.right_bound,
        }
    }
}

impl fmt::Debug for PayoutParameter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let left = self.left_bound;
        let right = self.right_bound;
        let short = self.short_amount;
        let long = self.long_amount;

        write!(f, "payout({left}..={right}, {short}, {long})",)
    }
}

/// Converts a float with any precision to a [`bitcoin::Amount`].
fn to_sats(btc: f64) -> Result<u64> {
    let sats_per_btc = Decimal::from(100_000_000);

    let btc = Decimal::from_f64(btc).context("Cannot create decimal from float")?;
    let sats = btc * sats_per_btc;
    let sats = sats.to_u64().context("Cannot fit sats into u64")?;

    Ok(sats)
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to init CSR object--is the specified shape correct?")]
    #[allow(clippy::upper_case_acronyms)]
    CannotInitCSR,
    #[error("matrix must be square")]
    MatrixMustBeSquare,
    #[error("cannot invert singular matrix")]
    SingularMatrix,
    #[error("evaluation outside parametric domain")]
    InvalidDomain,
    #[error("einsum error--array size mismatch?")]
    Einsum,
    #[error("no operand string found")]
    NoEinsumOperatorString,
    #[error("cannot connect periodic curves")]
    CannotConnectPeriodicCurves,
    #[error("degree must be strictly positive")]
    DegreeMustBePositive,
    #[error("all parameter arrays must have the same length if not using a tensor grid")]
    InvalidDerivative,
    #[error("Rational derivative not implemented for order sum(d) > 1")]
    DerivativeNotImplemented,
    #[error("requested segmentation is too coarse for this curve")]
    InvalidSegmentation,
    #[error("concatonation error")]
    NdArray {
        #[from]
        source: ndarray::ShapeError,
    },
    #[error(transparent)]
    NotOneDimensional {
        #[from]
        source: compat::NotOneDimensional,
    },
}

#[derive(Clone, Debug)]
struct PayoutCurve {
    curve: Curve,
    has_upper_limit: bool,
    total_value: f64,
}

impl PayoutCurve {
    fn new(
        initial_rate: f64,
        leverage_long: usize,
        leverage_short: usize,
        n_contracts: usize,
        contract_value: f64,
        tolerance: Option<f64>,
    ) -> Result<Self, Error> {
        let tolerance = tolerance.unwrap_or(1e-6);
        let bounds = cutoffs(initial_rate, leverage_long, leverage_short);
        let total_value = pool_value(
            initial_rate,
            n_contracts,
            contract_value,
            leverage_long,
            leverage_short,
        );
        let mut curve = curve_factory::line((0., 0.), (bounds.0, 0.), false)?;

        let payout =
            create_long_payout_function(initial_rate, n_contracts, contract_value, leverage_long);
        let variable_payout =
            curve_factory::fit(payout, bounds.0, bounds.1, Some(tolerance), None)?;
        curve.append(variable_payout)?;

        if bounds.2 {
            let upper_liquidation = curve_factory::line(
                (bounds.1, total_value),
                (4. * initial_rate, total_value),
                false,
            )?;
            curve.append(upper_liquidation)?;
        };

        Ok(PayoutCurve {
            curve,
            has_upper_limit: bounds.2,
            total_value,
        })
    }

    pub fn generate_payout_scheme(&self, n_segments: usize) -> Result<Array2<f64>, Error> {
        let n_min = if self.has_upper_limit { 3 } else { 2 };

        if n_segments < n_min {
            return Result::Err(Error::InvalidSegmentation);
        }

        let t = if self.has_upper_limit {
            self.build_sampling_vector_upper_bounded(n_segments)
        } else {
            self.build_sampling_vector_upper_unbounded(n_segments)
        };

        let mut z_arr = self.curve.evaluate(&mut &[t][..])?;
        if self.has_upper_limit {
            self.modify_samples_bounded(&mut z_arr);
        } else {
            self.modify_samples_unbounded(&mut z_arr);
        }
        self.generate_segments(&mut z_arr);

        Ok(z_arr)
    }

    fn build_sampling_vector_upper_bounded(&self, n_segs: usize) -> Array1<f64> {
        let knots = &self.curve.spline.knots(0, None)[0];
        let klen = knots.len();
        let n_64 = (n_segs + 1) as f64;
        let d = knots[klen - 2] - knots[1];
        let delta_0 = d / (2. * (n_64 - 5.));
        let delta_1 = d * (n_64 - 6.) / ((n_64 - 5.) * (n_64 - 4.));

        let mut vec = Vec::<f64>::with_capacity(n_segs + 2);
        for i in 0..n_segs + 2 {
            if i == 0 {
                vec.push(self.curve.spline.bases[0].start());
            } else if i == 1 {
                vec.push(knots[1]);
            } else if i == 2 {
                vec.push(knots[1] + delta_0);
            } else if i == n_segs - 1 {
                vec.push(knots[klen - 2] - delta_0);
            } else if i == n_segs {
                vec.push(knots[klen - 2]);
            } else if i == n_segs + 1 {
                vec.push(self.curve.spline.bases[0].end());
            } else {
                let c = (i - 2) as f64;
                vec.push(knots[1] + delta_0 + c * delta_1);
            }
        }
        Array1::<f64>::from_vec(vec)
    }

    fn build_sampling_vector_upper_unbounded(&self, n_segs: usize) -> Array1<f64> {
        let knots = &self.curve.spline.knots(0, None)[0];
        let klen = knots.len();
        let n_64 = (n_segs + 1) as f64;
        let d = knots[klen - 1] - knots[1];
        let delta = d / (n_64 - 1_f64);
        let delta_x = d / (2. * (n_64 - 1_f64));
        let delta_y = 3. * d / (2. * (n_64 - 1_f64));

        let mut vec = Vec::<f64>::with_capacity(n_segs + 2);
        for i in 0..n_segs + 2 {
            if i == 0 {
                vec.push(self.curve.spline.bases[0].start());
            } else if i == 1 {
                vec.push(knots[1]);
            } else if i == 2 {
                vec.push(knots[1] + delta_x);
            } else if i == n_segs {
                vec.push(knots[klen - 1] - delta_y);
            } else if i == n_segs + 1 {
                vec.push(knots[klen - 1]);
            } else {
                let c = (i - 2) as f64;
                vec.push(knots[1] + delta_x + c * delta);
            }
        }
        Array1::<f64>::from_vec(vec)
    }

    fn modify_samples_bounded(&self, arr: &mut Array2<f64>) {
        let n = arr.shape()[0];
        let capacity = 2 * (n - 2);
        let mut vec = Vec::<f64>::with_capacity(2 * capacity);
        for (i, e) in arr.slice(s![.., 0]).iter().enumerate() {
            if i < 2 || i > n - 3 {
                vec.push(*e);
            } else if i == 2 {
                vec.push(arr[[i - 1, 0]]);
                vec.push(arr[[i, 1]]);
                vec.push((*e + arr[[i + 1, 0]]) / 2.);
            } else if i == n - 3 {
                vec.push((arr[[i - 1, 0]] + *e) / 2.);
                vec.push(arr[[i, 1]]);
                vec.push(arr[[i + 1, 0]]);
            } else {
                vec.push((arr[[i - 1, 0]] + *e) / 2.);
                vec.push(arr[[i, 1]]);
                vec.push((*e + arr[[i + 1, 0]]) / 2.);
            }
            vec.push(arr[[i, 1]]);
        }

        *arr = Array2::<f64>::from_shape_vec((capacity, 2), vec).expect("vec is a 2D array");
    }

    fn modify_samples_unbounded(&self, arr: &mut Array2<f64>) {
        let n = arr.shape()[0];
        let capacity = 2 * (n - 1);
        let mut vec = Vec::<f64>::with_capacity(2 * capacity);
        for (i, e) in arr.slice(s![.., 0]).iter().enumerate() {
            if i < 2 {
                vec.push(*e);
            } else if i == 2 {
                vec.push(arr[[i - 1, 0]]);
                vec.push(arr[[i, 1]]);
                vec.push((*e + arr[[i + 1, 0]]) / 2.);
            } else if i == n - 1 {
                vec.push((arr[[i - 1, 0]] + *e) / 2.);
                vec.push(arr[[i, 1]]);
                vec.push(arr[[i, 0]]);
            } else {
                vec.push((arr[[i - 1, 0]] + *e) / 2.);
                vec.push(arr[[i, 1]]);
                vec.push((*e + arr[[i + 1, 0]]) / 2.);
            }
            vec.push(arr[[i, 1]]);
        }

        *arr = Array2::<f64>::from_shape_vec((capacity, 2), vec).expect("vec is a 2D array");
    }

    /// this should only be used on an array `arr` that has been
    /// processed by self.modify_samples_* first, otherwise the results
    /// will be jibberish.
    fn generate_segments(&self, arr: &mut Array2<f64>) {
        let capacity = 3 * arr.shape()[0] / 2;
        let mut vec = Vec::<f64>::with_capacity(capacity);
        for (i, e) in arr.slice(s![.., 0]).iter().enumerate() {
            if i == 0 {
                vec.push(e.floor());
            } else if i % 2 == 1 {
                vec.push(e.round());
                vec.push(arr[[i, 1]]);
            } else {
                vec.push(e.round() + 1_f64);
            }
        }

        *arr = Array2::<f64>::from_shape_vec((capacity / 3, 3), vec).expect("vec is a 2D array");
    }
}

fn cutoffs(initial_rate: f64, leverage_long: usize, leverage_short: usize) -> (f64, f64, bool) {
    let ll_64 = leverage_long as f64;
    let ls_64 = leverage_short as f64;
    let a = initial_rate * ll_64 / (ll_64 + 1_f64);
    if leverage_short == 1 {
        let b = 2. * initial_rate;
        return (a, b, false);
    }
    let b = initial_rate * ls_64 / (ls_64 - 1_f64);

    (a, b, true)
}

fn pool_value(
    initial_rate: f64,
    n_contracts: usize,
    contract_value: f64,
    leverage_long: usize,
    leverage_short: usize,
) -> f64 {
    let ll_64 = leverage_long as f64;
    let ls_64 = leverage_short as f64;
    let n_64 = n_contracts as f64;

    (n_64 * contract_value / initial_rate) * (1_f64 / ll_64 + 1_f64 / ls_64)
}

fn create_long_payout_function(
    initial_rate: f64,
    n_contracts: usize,
    contract_value: f64,
    leverage_long: usize,
) -> impl Fn(&Array1<f64>) -> Array2<f64> {
    let n_64 = n_contracts as f64;
    let ll_64 = leverage_long as f64;

    move |t: &Array1<f64>| {
        let mut vec = Vec::<f64>::with_capacity(2 * t.len());
        for e in t.iter() {
            let eval = (n_64 * contract_value)
                * (1_f64 / (initial_rate * ll_64) + (1_f64 / initial_rate - 1_f64 / e));
            vec.push(*e);
            vec.push(eval);
        }

        Array2::<f64>::from_shape_vec((t.len(), 2), vec).expect("vec is a 2D array")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk::bitcoin::Amount;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;
    use std::ops::RangeInclusive;

    #[test]
    fn test_bounded() {
        let initial_rate = 40000.0;
        let leverage_long = 5;
        let leverage_short = 2;
        let n_contracts = 200;
        let contract_value = 100.;

        let payout = PayoutCurve::new(
            initial_rate,
            leverage_long,
            leverage_short,
            n_contracts,
            contract_value,
            None,
        )
        .unwrap();

        let z = payout.generate_payout_scheme(5000).unwrap();

        assert!(z.shape()[0] == 5000);
    }

    #[test]
    fn test_unbounded() {
        let initial_rate = 40000.0;
        let leverage_long = 5;
        let leverage_short = 1;
        let n_contracts = 200;
        let contract_value = 100.;

        let payout = PayoutCurve::new(
            initial_rate,
            leverage_long,
            leverage_short,
            n_contracts,
            contract_value,
            None,
        )
        .unwrap();

        let z = payout.generate_payout_scheme(5000).unwrap();

        // out-by-one error expected at this point in time
        assert!(z.shape()[0] == 5001);
    }

    #[test]
    fn calculate_snapshot() {
        let actual_payouts = calculate_payout_parameters(
            Price::new(dec!(54000.00)).unwrap(),
            Usd::new(dec!(3500.00)),
            Leverage::new(5).unwrap(),
            Leverage::new(1).unwrap(),
            200,
            CompleteFee::None,
        )
        .unwrap();

        let expected_payouts = vec![
            payout(0..=45000, 7777777, 0),
            payout(45001..=45315, 7750759, 27018),
            payout(45316..=45630, 7697244, 80533),
            payout(45631..=45945, 7644417, 133359),
            payout(45946..=46260, 7592270, 185507),
            payout(46261..=46575, 7540793, 236984),
            payout(46576..=46890, 7489978, 287799),
            payout(46891..=47205, 7439816, 337961),
            payout(47206..=47520, 7390298, 387479),
            payout(47521..=47835, 7341415, 436362),
            payout(47836..=48150, 7293159, 484618),
            payout(48151..=48465, 7245520, 532257),
            payout(48466..=48780, 7198490, 579287),
            payout(48781..=49095, 7152060, 625717),
            payout(49096..=49410, 7106222, 671555),
            payout(49411..=49725, 7060965, 716812),
            payout(49726..=50040, 7016282, 761494),
            payout(50041..=50355, 6972164, 805612),
            payout(50356..=50670, 6928602, 849174),
            payout(50671..=50985, 6885587, 892189),
            payout(50986..=51300, 6843111, 934666),
            payout(51301..=51615, 6801163, 976613),
            payout(51616..=51930, 6759737, 1018040),
            payout(51931..=52245, 6718822, 1058955),
            payout(52246..=52560, 6678410, 1099367),
            payout(52561..=52875, 6638493, 1139284),
            payout(52876..=53190, 6599060, 1178716),
            payout(53191..=53505, 6560105, 1217672),
            payout(53506..=53820, 6521617, 1256160),
            payout(53821..=54135, 6483588, 1294189),
            payout(54136..=54450, 6446009, 1331768),
            payout(54451..=54765, 6408872, 1368905),
            payout(54766..=55080, 6372166, 1405610),
            payout(55081..=55395, 6335885, 1441892),
            payout(55396..=55710, 6300018, 1477758),
            payout(55711..=56025, 6264558, 1513219),
            payout(56026..=56340, 6229494, 1548282),
            payout(56341..=56655, 6194820, 1582957),
            payout(56656..=56970, 6160524, 1617253),
            payout(56971..=57285, 6126599, 1651177),
            payout(57286..=57600, 6093037, 1684740),
            payout(57601..=57915, 6059827, 1717949),
            payout(57916..=58230, 6026965, 1750812),
            payout(58231..=58545, 5994445, 1783332),
            payout(58546..=58860, 5962264, 1815512),
            payout(58861..=59175, 5930419, 1847358),
            payout(59176..=59490, 5898905, 1878872),
            payout(59491..=59805, 5867718, 1910059),
            payout(59806..=60120, 5836855, 1940922),
            payout(60121..=60435, 5806311, 1971465),
            payout(60436..=60750, 5776084, 2001693),
            payout(60751..=61065, 5746168, 2031608),
            payout(61066..=61380, 5716561, 2061216),
            payout(61381..=61695, 5687258, 2090519),
            payout(61696..=62010, 5658255, 2119522),
            payout(62011..=62325, 5629549, 2148228),
            payout(62326..=62640, 5601135, 2176642),
            payout(62641..=62955, 5573010, 2204767),
            payout(62956..=63270, 5545170, 2232607),
            payout(63271..=63585, 5517611, 2260165),
            payout(63586..=63900, 5490330, 2287447),
            payout(63901..=64215, 5463321, 2314455),
            payout(64216..=64530, 5436583, 2341194),
            payout(64531..=64845, 5410109, 2367667),
            payout(64846..=65160, 5383898, 2393879),
            payout(65161..=65475, 5357944, 2419833),
            payout(65476..=65790, 5332245, 2445532),
            payout(65791..=66105, 5306795, 2470982),
            payout(66106..=66420, 5281592, 2496185),
            payout(66421..=66735, 5256631, 2521146),
            payout(66736..=67050, 5231909, 2545868),
            payout(67051..=67365, 5207421, 2570356),
            payout(67366..=67680, 5183164, 2594612),
            payout(67681..=67995, 5159135, 2618642),
            payout(67996..=68310, 5135328, 2642449),
            payout(68311..=68625, 5111740, 2666037),
            payout(68626..=68940, 5088368, 2689409),
            payout(68941..=69255, 5065207, 2712569),
            payout(69256..=69570, 5042254, 2735523),
            payout(69571..=69885, 5019505, 2758272),
            payout(69886..=70200, 4996955, 2780821),
            payout(70201..=70515, 4974602, 2803175),
            payout(70516..=70830, 4952442, 2825335),
            payout(70831..=71145, 4930473, 2847304),
            payout(71146..=71460, 4908694, 2869083),
            payout(71461..=71775, 4887102, 2890675),
            payout(71776..=72090, 4865695, 2912081),
            payout(72091..=72405, 4844473, 2933304),
            payout(72406..=72720, 4823433, 2954344),
            payout(72721..=73035, 4802573, 2975204),
            payout(73036..=73350, 4781891, 2995886),
            payout(73351..=73665, 4761385, 3016391),
            payout(73666..=73980, 4741054, 3036722),
            payout(73981..=74295, 4720896, 3056881),
            payout(74296..=74610, 4700909, 3076868),
            payout(74611..=74925, 4681090, 3096686),
            payout(74926..=75240, 4661439, 3116338),
            payout(75241..=75555, 4641953, 3135824),
            payout(75556..=75870, 4622630, 3155146),
            payout(75871..=76185, 4603469, 3174307),
            payout(76186..=76500, 4584468, 3193309),
            payout(76501..=76815, 4565624, 3212153),
            payout(76816..=77130, 4546937, 3230840),
            payout(77131..=77445, 4528403, 3249374),
            payout(77446..=77760, 4510022, 3267755),
            payout(77761..=78075, 4491791, 3285986),
            payout(78076..=78390, 4473708, 3304068),
            payout(78391..=78705, 4455773, 3322004),
            payout(78706..=79020, 4437982, 3339795),
            payout(79021..=79335, 4420333, 3357443),
            payout(79336..=79650, 4402827, 3374950),
            payout(79651..=79965, 4385459, 3392318),
            payout(79966..=80280, 4368228, 3409548),
            payout(80281..=80595, 4351133, 3426643),
            payout(80596..=80910, 4334172, 3443605),
            payout(80911..=81225, 4317343, 3460434),
            payout(81226..=81540, 4300643, 3477134),
            payout(81541..=81855, 4284071, 3493705),
            payout(81856..=82170, 4267626, 3510151),
            payout(82171..=82485, 4251305, 3526472),
            payout(82486..=82800, 4235107, 3542670),
            payout(82801..=83115, 4219029, 3558748),
            payout(83116..=83430, 4203070, 3574707),
            payout(83431..=83745, 4187229, 3590547),
            payout(83746..=84060, 4171506, 3606271),
            payout(84061..=84375, 4155899, 3621878),
            payout(84376..=84690, 4140406, 3637371),
            payout(84691..=85005, 4125028, 3652749),
            payout(85006..=85320, 4109763, 3668014),
            payout(85321..=85635, 4094610, 3683167),
            payout(85636..=85950, 4079567, 3698209),
            payout(85951..=86265, 4064635, 3713142),
            payout(86266..=86580, 4049812, 3727965),
            payout(86581..=86895, 4035096, 3742680),
            payout(86896..=87210, 4020488, 3757289),
            payout(87211..=87525, 4005985, 3771792),
            payout(87526..=87840, 3991587, 3786189),
            payout(87841..=88155, 3977293, 3800484),
            payout(88156..=88470, 3963102, 3814675),
            payout(88471..=88785, 3949013, 3828764),
            payout(88786..=89100, 3935024, 3842753),
            payout(89101..=89415, 3921135, 3856642),
            payout(89416..=89730, 3907344, 3870432),
            payout(89731..=90045, 3893652, 3884125),
            payout(90046..=90360, 3880056, 3897721),
            payout(90361..=90675, 3866555, 3911221),
            payout(90676..=90990, 3853150, 3924627),
            payout(90991..=91305, 3839837, 3937940),
            payout(91306..=91620, 3826618, 3951159),
            payout(91621..=91935, 3813489, 3964287),
            payout(91936..=92250, 3800452, 3977325),
            payout(92251..=92565, 3787504, 3990273),
            payout(92566..=92880, 3774644, 4003133),
            payout(92881..=93195, 3761872, 4015905),
            payout(93196..=93510, 3749186, 4028591),
            payout(93511..=93825, 3736585, 4041192),
            payout(93826..=94140, 3724069, 4053708),
            payout(94141..=94455, 3711636, 4066141),
            payout(94456..=94770, 3699286, 4078491),
            payout(94771..=95085, 3687016, 4090760),
            payout(95086..=95400, 3674827, 4102949),
            payout(95401..=95715, 3662718, 4115059),
            payout(95716..=96030, 3650686, 4127091),
            payout(96031..=96345, 3638733, 4139044),
            payout(96346..=96660, 3626856, 4150920),
            payout(96661..=96975, 3615057, 4162720),
            payout(96976..=97290, 3603333, 4174444),
            payout(97291..=97605, 3591684, 4186093),
            payout(97606..=97920, 3580110, 4197666),
            payout(97921..=98235, 3568611, 4209166),
            payout(98236..=98550, 3557184, 4220593),
            payout(98551..=98865, 3545831, 4231946),
            payout(98866..=99180, 3534550, 4243227),
            payout(99181..=99495, 3523340, 4254437),
            payout(99496..=99810, 3512201, 4265576),
            payout(99811..=100125, 3501133, 4276644),
            payout(100126..=100440, 3490134, 4287643),
            payout(100441..=100755, 3479205, 4298572),
            payout(100756..=101070, 3468344, 4309433),
            payout(101071..=101385, 3457551, 4320226),
            payout(101386..=101700, 3446825, 4330952),
            payout(101701..=102015, 3436165, 4341611),
            payout(102016..=102330, 3425572, 4352205),
            payout(102331..=102645, 3415044, 4362732),
            payout(102646..=102960, 3404581, 4373196),
            payout(102961..=103275, 3394182, 4383594),
            payout(103276..=103590, 3383847, 4393930),
            payout(103591..=103905, 3373574, 4404202),
            payout(103906..=104220, 3363364, 4414412),
            payout(104221..=104535, 3353216, 4424561),
            payout(104536..=104850, 3343128, 4434648),
            payout(104851..=105165, 3333101, 4444675),
            payout(105166..=105480, 3323134, 4454643),
            payout(105481..=105795, 3313226, 4464551),
            payout(105796..=106110, 3303377, 4474400),
            payout(106111..=106425, 3293585, 4484192),
            payout(106426..=106740, 3283851, 4493926),
            payout(106741..=107055, 3274174, 4503603),
            payout(107056..=107370, 3264552, 4513225),
            payout(107371..=107764, 3254986, 4522790),
            payout(107765..=108000, 3240740, 4537037),
        ];

        pretty_assertions::assert_eq!(actual_payouts, expected_payouts);
    }

    #[test]
    fn verify_effect_of_funding_fee() {
        let price = Price::new(dec!(54000.00)).unwrap();
        let quantity = Usd::new(dec!(3500.00));

        let payouts = calculate_payout_parameters(
            price,
            quantity,
            Leverage::new(5).unwrap(),
            Leverage::new(1).unwrap(),
            200,
            CompleteFee::None,
        )
        .unwrap();

        let fee = CompleteFee::LongPaysShort(Amount::from_sat(100));

        let payouts_with_fee = calculate_payout_parameters(
            price,
            quantity,
            Leverage::new(5).unwrap(),
            Leverage::new(1).unwrap(),
            200,
            fee,
        )
        .unwrap();
        assert_eq!(payouts.len(), payouts_with_fee.len());

        let total_per_payout = payouts
            .iter()
            .map(|payout| payout.short_amount + payout.long_amount)
            .collect::<Vec<_>>();
        let total_per_payout_with_fee = payouts_with_fee
            .iter()
            .map(|payout| payout.short_amount + payout.long_amount)
            .collect::<Vec<_>>();
        assert_eq!(total_per_payout, total_per_payout_with_fee);

        let fee: u64 = match fee {
            CompleteFee::LongPaysShort(fee) => fee.as_sat(),
            _ => unreachable!("unreachable in this test"),
        };

        let fees = payouts
            .iter()
            .zip(payouts_with_fee)
            .map(|(payout, payout_with_fee)| payout_with_fee.short_amount - payout.short_amount)
            .collect::<Vec<_>>();
        let expected_fees = vec![
            0, // the lower tail of this curve already assigns the short side all of the coins
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
            fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee, fee,
        ];

        assert_eq!(fees, expected_fees);
    }

    #[test]
    fn verify_tails() {
        let actual_payouts = calculate_payout_parameters(
            Price::new(dec!(54000.00)).unwrap(),
            Usd::new(dec!(3500.00)),
            Leverage::new(5).unwrap(),
            Leverage::new(1).unwrap(),
            200,
            CompleteFee::None,
        )
        .unwrap();

        let lower_tail = payout(0..=45000, 7777777, 0);
        let upper_tail = payout(107765..=108000, 3240740, 4537037);

        pretty_assertions::assert_eq!(actual_payouts.first().unwrap(), &lower_tail);
        pretty_assertions::assert_eq!(actual_payouts.last().unwrap(), &upper_tail);
    }

    proptest! {
        /// By similar we mean that they're at most 1 satoshi off the
        /// next payout sum.
        #[test]
        fn payout_totals_are_similar(
            price in arb_price(1.0, 340_000.0),
            n_contracts in arb_contracts(1, 10_000_000),
            long_leverage in arb_leverage(1, 200),
            short_leverage in arb_leverage(1, 200),
            n_payouts in 10usize..2000,
            fee_flow in arb_fee_flow(-100_000_000, 100_000_000),
        ) {
            let payouts = calculate_payout_parameters(
                price,
                n_contracts,
                long_leverage,
                short_leverage,
                n_payouts,
                fee_flow,
            )
            .unwrap();

            let are_payout_totals_similar = payouts
                .iter()
                .zip(payouts.iter().skip(1))
                .map(|(a, b)| {
                    (
                        (a.long_amount + a.short_amount),
                        (b.long_amount + b.short_amount),
                    )
                })
                .all(|(a, b)| (a as i64 - b as i64).abs() <= 1);

            prop_assert!(are_payout_totals_similar)
        }
    }

    prop_compose! {
        fn arb_price(min: f64, max: f64)(price in min..max) -> Price {
            let price = Decimal::from_f64(price).unwrap();

            Price::new(price).unwrap()
        }
    }

    prop_compose! {
        fn arb_contracts(min: u64, max: u64)(contracts in min..max) -> Usd {
            let contracts = Decimal::from_u64(contracts).unwrap();

            Usd::new(contracts)
        }
    }

    prop_compose! {
        fn arb_leverage(min: u8, max: u8)(leverage in min..max) -> Leverage {
            Leverage::new(leverage).unwrap()
        }
    }

    prop_compose! {
        /// Generate an arbitrary fee flow value, between the `lower`
        /// and `upper` bounds.
        ///
        /// A positive value represents a fee flow from long to short.
        /// Conversely, a negative valure represents a fee flow from
        /// short to long.
        fn arb_fee_flow(lower: i64, upper: i64)(fee in lower..upper) -> CompleteFee {
            let fee_amount = Amount::from_sat(fee.abs().try_into().unwrap());
            if fee.is_positive() {
                CompleteFee::LongPaysShort(fee_amount)
            } else if fee.is_negative() {
                CompleteFee::ShortPaysLong(fee_amount)
            } else {
                CompleteFee::None
            }
        }
    }

    fn payout(range: RangeInclusive<u64>, short: u64, long: u64) -> PayoutParameter {
        PayoutParameter {
            left_bound: *range.start(),
            right_bound: *range.end(),
            long_amount: long,
            short_amount: short,
        }
    }
}
