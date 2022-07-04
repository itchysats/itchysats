use crate::payouts::payout_curve::basis::BSplineBasis;
use crate::payouts::payout_curve::curve::Curve;
use crate::payouts::payout_curve::utils::cmp_f64;
use crate::payouts::payout_curve::Error;
use ndarray::prelude::*;

/// Perform general spline interpolation on a provided basis.
///
/// ### parameters
/// * x: Matrix *X\[i,j\]* of interpolation points *x_i* with components *j*
/// * basis: Basis on which to interpolate
/// * t: parametric values at interpolation points; defaults to
/// Greville points if not provided
///
/// ### returns
/// * Interpolated curve
pub fn interpolate(
    x: &Array2<f64>,
    basis: &BSplineBasis,
    t: Option<Array1<f64>>,
) -> Result<Curve, Error> {
    let mut t = t.unwrap_or_else(|| basis.greville());
    let evals = basis.evaluate(&mut t, 0, true)?;
    let controlpoints = evals.matrix_solve(x)?;
    let out = Curve::new(Some(vec![basis.clone()]), Some(controlpoints), None)?;

    Ok(out)
}

/// Computes an interpolation for a parametric curve up to a specified
/// tolerance. The method will iteratively refine parts where needed
/// resulting in a non-uniform knot vector with as optimized knot
/// locations as possible.
///
/// ### parameters
/// * x: callable function `x: t --> (t, x(t))` which takes as input a vector
/// of evaluation points `t` and gives as output a matrix `x` where `x\[i,j\]`
/// is component `j` evaluated at point `t\[i\]`
/// * t0: start of parametric domain
/// * t1: end of parametric domain
/// * rtol: relative tolerance for stopping criterium. It is defined to be
/// `||e||_L2 / D`, where `D` is the length of the curve and `||e||_L2` is
/// the L2-error (see Curve.error)
/// * atol: absolute tolerance for stopping criterium. It is defined to be
/// the maximal distance between the curve approximation and the exact curve
///
/// ### returns
/// Curve (NURBS)
pub fn fit(
    x: impl Fn(&Array1<f64>) -> Array2<f64>,
    t0: f64,
    t1: f64,
    rtol: Option<f64>,
    atol: Option<f64>,
) -> Result<Curve, Error> {
    let rtol = rtol.unwrap_or(1e-4);
    let atol = atol.unwrap_or(0.0);

    let knot_vector = Array1::<f64>::from_vec(vec![t0, t0, t0, t0, t1, t1, t1, t1]);
    let b = BSplineBasis::new(Some(4), Some(knot_vector), None)?;
    let t = b.greville();
    let exact = &x(&t);

    let mut crv = interpolate(exact, &b, Some(t))?;
    let err = crv.error(&x)?;

    // polynomial input (which can be exactly represented) only use one knot span
    if err.1 < 1e-13 {
        return Ok(crv);
    }

    // for all other curves, start with 4 knot spans
    let mut knot_vec = Vec::<f64>::with_capacity(12);
    for _ in 0..4 {
        knot_vec.push(t0)
    }
    for i in 0..4 {
        let i_64 = (i + 1) as f64;
        let val = i_64 / 5. * (t1 - t0) + t0;
        knot_vec.push(val);
    }
    for _ in 0..4 {
        knot_vec.push(t1)
    }
    let knot_vector = Array1::<f64>::from_vec(knot_vec.clone());
    let b = BSplineBasis::new(Some(4), Some(knot_vector), None)?;
    let t = b.greville();
    let exact = &x(&t);

    crv = interpolate(exact, &b, Some(t))?;
    let err = crv.error(&x)?;
    let mut err_l2 = err.0;
    let mut err_max = err.1;

    // this is technically false since we need the length of the target function *x*
    // and not our approximation *crv*, but we don't have the derivative of *x*, so
    // we can't compute it. This seems like a healthy compromise
    let length = crv.length(None, None)?;
    let mut target = (err_l2.sum() / length).sqrt();

    // conv_order = 4
    // square_conv_order = 2 * conv_order
    // scale = square_conv_order + 4
    let scale_64 = 12_f64;

    while target > rtol && err_max > atol {
        let knot_span = &crv.spline.knots(0, Some(false))[0];
        let target_error = (rtol * length).powi(2) / err_l2.len() as f64;
        for i in 0..err_l2.len() {
            // figure out how many new knots we require in this knot interval:
            // if we converge with *scale* and want an error of *target_error*
            // |e|^2 * (1/n)^scale = target_error^2
            let n = ((err_l2[i].ln() - target_error.ln()) / scale_64)
                .exp()
                .ceil() as usize;

            // add *n* new interior knots to this knot span
            let new_knots = Array1::<f64>::linspace(knot_span[i], knot_span[i + 1], n + 1);
            for e in new_knots.slice(s![1..new_knots.len() - 1]).iter() {
                knot_vec.push(*e);
            }
        }

        // build new refined knot vector
        knot_vec.sort_by(cmp_f64);
        let knot_vector = Array1::<f64>::from_vec(knot_vec.clone());
        let b = BSplineBasis::new(Some(4), Some(knot_vector), None)?;

        // do interpolation and return result
        let t = b.greville();
        let exact = &x(&t);

        crv = interpolate(exact, &b, Some(t))?;
        let err = crv.error(&x)?;
        err_l2 = err.0;
        err_max = err.1;
        target = err_l2.sum().sqrt() / length;
    }

    Ok(crv)
}

/// Create a line between two points.
///
/// ### parameters
/// * a, b: start and end points (resp.)
/// * relative: whether `b` is relative to `a` or absolute
pub fn line(a: (f64, f64), b: (f64, f64), relative: bool) -> Result<Curve, Error> {
    let vec = if relative {
        vec![[a.0, a.1], [a.0 + b.0, a.1 + b.1]]
    } else {
        vec![[a.0, a.1], [b.0, b.1]]
    };
    let controlpoints = Array2::<f64>::from(vec);

    Curve::new(None, Some(controlpoints), None)
}
