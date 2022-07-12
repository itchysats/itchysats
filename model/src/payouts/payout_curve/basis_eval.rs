use crate::payouts::payout_curve::csr_tools::CSR;
use crate::payouts::payout_curve::utils::*;
use crate::payouts::payout_curve::Error;
use ndarray::prelude::*;
use std::cmp::min;

#[derive(Clone, Debug)]
pub struct Basis {
    pub knots: Array1<f64>,
    pub order: usize,
    pub periodic: isize,
    n_all: usize,
    n: usize,
    pub start: f64,
    pub end: f64,
    pub ktol: f64,
}

impl Basis {
    pub fn new(
        order: usize,
        knots: Array1<f64>,
        periodic: Option<isize>,
        knot_tol: Option<f64>,
    ) -> Self {
        let p = periodic.unwrap_or(-1);
        let n_all = knots.len() - order;
        let n = knots.len() - order - ((p + 1) as usize);
        let start = knots[order - 1];
        let end = knots[n_all];

        Basis {
            order,
            knots,
            periodic: p,
            n_all,
            n,
            start,
            end,
            ktol: knot_tolerance(knot_tol),
        }
    }

    /// Wrap periodic evaluation into domain
    fn wrap_periodic(&self, t: &mut Array1<f64>, right: &bool) {
        for i in 0..t.len() {
            if t[i] < self.start || t[i] > self.end {
                t[i] = (t[i] - self.start) % (self.end - self.start) + self.start;
            }
            if (t[i] - self.start).abs() < self.ktol && !right {
                t[i] = self.end;
            }
        }
    }

    pub fn evaluate(
        &self,
        t: &mut Array1<f64>,
        d: usize,
        from_right: Option<bool>,
    ) -> Result<CSR, Error> {
        let m = t.len();
        let mut right = from_right.unwrap_or(true);

        if self.periodic >= 0 {
            self.wrap_periodic(t, &right);
        }

        let mut store = Array1::<f64>::zeros(self.order);
        let mut mu;
        let mut idx_j;
        let mut idx_k;

        let mut data = Array1::<f64>::zeros(m * self.order);
        let mut indices = Array1::<usize>::zeros(m * self.order);
        let indptr =
            Array1::<usize>::from_vec((0..m * self.order + 1).step_by(self.order).collect());

        for i in 0..m {
            right = from_right.unwrap_or(true);
            // Special-case the endpoint, so the user doesn't need to
            if (t[i] - self.end).abs() < self.ktol {
                right = false;
            }
            // Skip non-periodic evaluation points outside the domain
            if t[i] < self.start
                || t[i] > self.end
                || ((t[i] - self.start).abs() < self.ktol && !right)
            {
                continue;
            }

            // mu = index of last non-zero basis function
            if right {
                mu = bisect_right(&self.knots, &t[i], self.n_all + self.order);
            } else {
                mu = bisect_left(&self.knots, &t[i], self.n_all + self.order);
            }
            mu = min(mu, self.n_all);

            for k in 0..self.order - 1 {
                store[k] = 0.;
            }

            // the last entry is a dummy-zero which is never used
            store[self.order - 1] = 1.;

            for q in 1..self.order - d {
                idx_j = self.order - q - 1;
                idx_k = mu - q - 1;
                store[idx_j] += store[idx_j + 1] * (self.knots[idx_k + q + 1] - t[i])
                    / (self.knots[idx_k + q + 1] - self.knots[idx_k + 1]);

                for j in self.order - q..self.order - 1 {
                    // 'i'-index in global knot vector (ref Hughes book pg.21)
                    let k = mu - self.order + j;
                    store[j] =
                        store[j] * (t[i] - self.knots[k]) / (self.knots[k + q] - self.knots[k]);
                    store[j] += store[j + 1] * (self.knots[k + q + 1] - t[i])
                        / (self.knots[k + q + 1] - self.knots[k + 1]);
                }
                idx_j = self.order - 1;
                idx_k = mu - 1;
                store[idx_j] = store[idx_j] * (t[i] - self.knots[idx_k])
                    / (self.knots[idx_k + q] - self.knots[idx_k]);
            }

            for q in self.order - d..self.order {
                for j in self.order - q - 1..self.order {
                    // 'i'-index in global knot vector (ref Hughes book pg.21)
                    idx_k = mu - self.order + j;
                    if j != self.order - q - 1 {
                        store[j] =
                            store[j] * (q as f64) / (self.knots[idx_k + q] - self.knots[idx_k]);
                    }
                    if j != self.order - 1 {
                        store[j] -= store[j + 1] * (q as f64)
                            / (self.knots[idx_k + q + 1] - self.knots[idx_k + 1]);
                    }
                }
            }

            for (j, k) in (i * self.order..(i + 1) * self.order).enumerate() {
                data[k] = store[j];
                indices[k] = (mu - self.order + j) % self.n;
            }
        }

        let csr = CSR::new(data, indices, indptr, (m, self.n))?;

        Ok(csr)
    }
}

pub fn knot_tolerance(tol: Option<f64>) -> f64 {
    tol.unwrap_or(1e-10)
}

/// Snap evaluation points to knots if they are sufficiently close
/// as specified by self.ktol
///
/// * t: The parametric coordinate(s) in which to evaluate
/// * knots: knot-vector
/// * knot_tol: default=1e-10
pub fn snap(t: &mut Array1<f64>, knots: &Array1<f64>, knot_tol: Option<f64>) {
    let ktol = knot_tolerance(knot_tol);

    for j in 0..t.len() {
        let i = bisect_left(knots, &t[j], knots.len());
        if i < knots.len() && (knots[i] - t[j]).abs() < ktol {
            t[j] = knots[i];
        } else if i > 0 && (knots[i - 1] - t[j]).abs() < ktol {
            t[j] = knots[i - 1];
        }
    }
}
