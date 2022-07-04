use crate::payouts::payout_curve::basis_eval::*;
use crate::payouts::payout_curve::csr_tools::CSR;
use crate::payouts::payout_curve::utils::*;
use crate::payouts::payout_curve::Error;
use core::cmp::max;
use ndarray::concatenate;
use ndarray::prelude::*;
use ndarray::s;

#[derive(Clone, Debug)]
pub struct BSplineBasis {
    pub knots: Array1<f64>,
    pub order: usize,
    pub periodic: isize,
    pub knot_tol: f64,
}

impl BSplineBasis {
    pub fn new(
        order: Option<usize>,
        knots: Option<Array1<f64>>,
        periodic: Option<isize>,
    ) -> Result<Self, Error> {
        let order = order.unwrap_or(2);
        let periodic = periodic.unwrap_or(-1);
        let knots = match knots {
            Some(knots) => knots,
            None => default_knot(order, periodic)?,
        };
        let ktol = knot_tolerance(None);

        Ok(BSplineBasis {
            order,
            knots,
            periodic,
            knot_tol: ktol,
        })
    }

    pub fn num_functions(&self) -> usize {
        let p = (self.periodic + 1) as usize;

        self.knots.len() - self.order - p
    }

    /// Start point of parametric domain. For open knot vectors, this is the
    /// first knot.
    pub fn start(&self) -> f64 {
        self.knots[self.order - 1]
    }

    /// End point of parametric domain. For open knot vectors, this is the
    /// last knot.
    pub fn end(&self) -> f64 {
        self.knots[self.knots.len() - self.order]
    }

    /// Fetch greville points, also known as knot averages
    /// over entire knot vector:
    /// .. math:: \\sum_{j=i+1}^{i+p-1} \\frac{t_j}{p-1}
    pub fn greville(&self) -> Array1<f64> {
        let n = self.num_functions() as i32;

        (0..n).map(|idx| self.greville_single(idx)).collect()
    }

    fn greville_single(&self, index: i32) -> f64 {
        let p = self.order as i32;
        let den = (self.order - 1) as f64;

        self.knots.slice(s![index + 1..index + p]).sum() / den
    }

    /// Evaluate all basis functions in a given set of points.
    /// ## parameters:
    /// * t: The parametric coordinate(s) in which to evaluate
    /// * d: Number of derivatives to compute
    /// * from_right: true if evaluation should be done in the limit from above
    /// ## returns:
    /// * CSR (sparse) matrix N\[i,j\] of all basis functions j evaluated in all points j
    pub fn evaluate(&self, t: &mut Array1<f64>, d: usize, from_right: bool) -> Result<CSR, Error> {
        let basis = Basis::new(
            self.order,
            self.knots.clone().to_owned(),
            Some(self.periodic),
            Some(self.knot_tol),
        );
        snap(t, &basis.knots, Some(basis.ktol));

        if self.order <= d {
            let csr = CSR::new(
                Array1::<f64>::zeros(0),
                Array1::<usize>::zeros(0),
                Array1::<usize>::zeros(t.len() + 1),
                (t.len(), self.num_functions()),
            )?;

            return Ok(csr);
        }

        let out = basis.evaluate(t, d, Some(from_right))?;

        Ok(out)
    }

    /// Snap evaluation points to knots if they are sufficiently close
    /// as given in by knot_tolerance.
    ///
    /// * t: The parametric coordinate(s) in which to evaluate
    pub fn snap(&self, t: &mut Array1<f64>) {
        snap(t, &self.knots, Some(self.knot_tol))
    }

    /// Create a knot vector with higher order.
    ///
    /// The continuity at the knots are kept unchanged by increasing their
    /// multiplicities.
    ///
    /// ### parameters
    /// * amount: relative (polynomial) degree to raise the basis function by
    pub fn raise_order(&mut self, amount: usize) {
        if amount > 0 {
            let knot_spans_arr = self.knot_spans(true);
            let knot_spans = knot_spans_arr.iter().collect::<Vec<_>>();
            let temp = self.knots.clone();
            let mut knots = temp.iter().collect::<Vec<_>>();

            for _ in 0..amount {
                knots.append(&mut knot_spans.clone());
            }

            let mut knots_vec = knots.iter().map(|e| **e).collect::<Vec<_>>();
            knots_vec.sort_by(cmp_f64);

            let knots_arr = Array1::<f64>::from_vec(knots_vec);

            let new_knot = if self.periodic > -1 {
                let n_0 = bisect_left(&knots_arr, &self.start(), knots_arr.len());
                let n_1 =
                    knot_spans.len() - bisect_left(&knots_arr, &self.end(), knots_arr.len()) - 1;
                let mut new_knot_vec = knots[n_0 * amount..n_1 * amount]
                    .iter()
                    .map(|e| **e)
                    .collect::<Vec<_>>();
                new_knot_vec.sort_by(cmp_f64);
                Array1::<f64>::from_vec(new_knot_vec)
            } else {
                knots_arr
            };

            self.order += amount;
            self.knots = new_knot;
        }
    }

    /// Return the set of unique knots in the knot vector.
    ///
    /// ### parameters:
    /// * include_ghosts: if knots outside start/end are to be included. These \
    /// knots are used by periodic basis.
    ///
    /// ### returns:
    /// * 1-D array of unique knots
    pub fn knot_spans(&self, include_ghosts: bool) -> Array1<f64> {
        let p = &self.order;

        // XXX this is VERY sloppy!
        let mut res: Vec<f64> = vec![];
        if include_ghosts {
            res.push(self.knots[0]);
            for elem in self.knots.slice(s![1..]).iter() {
                if (elem - res[res.len() - 1]).abs() > self.knot_tol {
                    res.push(*elem);
                }
            }
        } else {
            res.push(self.knots[p - 1]);
            let klen = self.knots.len();
            for elem in self.knots.slice(s![p - 1..klen - p + 1]).iter() {
                if (elem - res[res.len() - 1]).abs() > self.knot_tol {
                    res.push(*elem);
                }
            }
        }

        Array1::<f64>::from_vec(res)
    }
}

fn default_knot(order: usize, periodic: isize) -> Result<Array1<f64>, Error> {
    let prd = max(periodic, -1);
    let p = (prd + 1) as usize;
    let mut knots = concatenate(
        Axis(0),
        &[
            Array1::<f64>::zeros(order).view(),
            Array1::<f64>::ones(order).view(),
        ],
    )?;

    for i in 0..p {
        knots[i] = -1.;
        knots[2 * order - i - 1] = 2.;
    }

    Ok(knots)
}
