use crate::payouts::payout_curve::Error;
use ndarray::prelude::*;
use ndarray::s;
use std::cmp::Ordering;
use std::f64::consts::PI;

pub fn bisect_left(arr: &Array1<f64>, val: &f64, mut hi: usize) -> usize {
    let mut lo: usize = 0;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if arr[mid] < *val {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    lo
}

pub fn bisect_right(arr: &Array1<f64>, val: &f64, mut hi: usize) -> usize {
    let mut lo: usize = 0;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if *val < arr[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    lo
}

pub fn cmp_f64(a: &f64, b: &f64) -> Ordering {
    if a < b {
        return Ordering::Less;
    } else if a > b {
        return Ordering::Greater;
    }
    Ordering::Equal
}

/// Gauss-Legendre_Quadrature
///
/// Could not find a rust implementation of this, so have created one from
/// a C implementation found
/// [here](https://rosettacode.org/wiki/Numerical_integration/Gauss-Legendre_Quadrature#C).
///
/// The code is well short of optimal, but it gets things moving. Better
/// versions are provided by, for example,
/// [numpy.polynomial.legendre.leggauss](https://github.com/numpy/numpy/blob/v1.21.0/numpy/polynomial/legendre.py#L1519-L1584)
/// but the implementaitons are more involved so we have opted for quick and
/// dirty for the time being.
#[derive(Debug, Clone)]
pub struct GaussLegendreQuadrature {
    pub sample_points: Array1<f64>,
    pub weights: Array1<f64>,
}

impl GaussLegendreQuadrature {
    pub fn new(order: usize) -> Result<Self, Error> {
        if order < 1 {
            return Err(Error::DegreeMustBePositive);
        }

        let data = legendre_wrapper(&order);

        Ok(GaussLegendreQuadrature {
            sample_points: data.0,
            weights: data.1,
        })
    }
}

fn legendre_wrapper(order: &usize) -> (Array1<f64>, Array1<f64>) {
    let arr = legendre_coefficients(order);
    legendre_roots(&arr)
}

fn legendre_coefficients(order: &usize) -> Array2<f64> {
    let mut lcoef_arr = Array2::<f64>::zeros((*order + 1, *order + 1));
    lcoef_arr[[0, 0]] = 1.;
    lcoef_arr[[1, 1]] = 1.;

    for n in 2..*order + 1 {
        let n_64 = n as f64;
        lcoef_arr[[n, 0]] = -(n_64 - 1.) * lcoef_arr[[n - 2, 0]] / n_64;

        for i in 1..n + 1 {
            lcoef_arr[[n, i]] = ((2. * n_64 - 1.) * lcoef_arr[[n - 1, i - 1]]
                - (n_64 - 1.) * lcoef_arr[[n - 2, i]])
                / n_64;
        }
    }

    lcoef_arr
}

fn legendre_eval(coeff_arr: &Array2<f64>, n: &usize, x: &f64) -> f64 {
    let mut s = coeff_arr[[*n, *n]];
    for i in (1..*n + 1).rev() {
        s = s * (*x) + coeff_arr[[*n, i - 1]];
    }

    s
}

fn legendre_diff(coeff_arr: &Array2<f64>, n: &usize, x: &f64) -> f64 {
    let n_64 = *n as f64;
    n_64 * (x * legendre_eval(coeff_arr, n, x) - legendre_eval(coeff_arr, &(n - 1), x))
        / (x * x - 1.)
}

fn legendre_roots(coeff_arr: &Array2<f64>) -> (Array1<f64>, Array1<f64>) {
    let n = coeff_arr.shape()[0] - 1;
    let n_64 = n as f64;

    let mut sample_points_arr = Array1::<f64>::zeros(n + 1);
    let mut weights_arr = Array1::<f64>::zeros(n + 1);

    for i in 1..n + 1 {
        let i_64 = i as f64;
        let mut x = (PI * (i_64 - 0.25) / (n_64 + 0.5)).cos();
        let mut x1 = x;
        x -= legendre_eval(coeff_arr, &n, &x) / legendre_diff(coeff_arr, &n, &x);

        while fdim(&x, &x1) > 2e-16 {
            x1 = x;
            x -= legendre_eval(coeff_arr, &n, &x) / legendre_diff(coeff_arr, &n, &x);
        }

        sample_points_arr[i - 1] = x;
        x1 = legendre_diff(coeff_arr, &n, &x);
        weights_arr[i - 1] = 2. / ((1. - x * x) * x1 * x1);
    }

    // truncate the dummy value off the end + reverse sample points +
    // use symmetry to stable things up a bit.
    let mut samples = sample_points_arr.slice(s![..n; -1]).to_owned();
    samples = symmetric_samples(&samples);

    let mut weights = weights_arr.slice(s![..n]).to_owned();
    weights = symmetric_weights(&weights);

    (samples, weights)
}

fn symmetric_samples(arr: &Array1<f64>) -> Array1<f64> {
    let arr_rev = arr.slice(s![..; -1]).to_owned();
    (arr - &arr_rev) / 2.
}

fn symmetric_weights(arr: &Array1<f64>) -> Array1<f64> {
    let s = &arr.sum_axis(Axis(0));
    let arr_rev = arr.slice(s![..; -1]).to_owned();
    (arr + &arr_rev) / s
}

fn fdim(a: &f64, b: &f64) -> f64 {
    if a - b > 0f64 {
        a - b
    } else {
        0f64
    }
}
