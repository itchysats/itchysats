use crate::payouts::payout_curve::basis::BSplineBasis;
use crate::payouts::payout_curve::splineobject::SplineObject;
use crate::payouts::payout_curve::utils::*;
use crate::payouts::payout_curve::Error;
use ndarray::prelude::*;
use ndarray::s;
use std::cmp::max;

fn default_basis() -> Result<Vec<BSplineBasis>, Error> {
    let out = vec![BSplineBasis::new(None, None, None)?];
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct Curve {
    pub spline: SplineObject,
}

/// Represents a curve: an object with a one-dimensional parameter space.
impl Curve {
    /// Construct a curve with the given basis and control points. In theory,
    /// the curve could be defined in some Euclidean space of dimension N, but
    /// for the moment only curves in E^2 are supported via hard-coding. That is,
    /// any valid basis set can be passed in when instantiating this object,
    /// but only the first one is every considered in the methods provided.
    ///
    /// The default is to create a linear one-element mapping from (0,1) to the
    /// unit interval.
    ///
    /// ### parameters
    /// * basis: The underlying B-Spline basis
    /// * controlpoints: An *n* × *d* matrix of control points
    /// * rational: Whether the curve is rational (in which case the
    /// control points are interpreted as pre-multiplied with the weight,
    /// which is the last coordinate)
    pub fn new(
        bases: Option<Vec<BSplineBasis>>,
        controlpoints: Option<Array2<f64>>,
        rational: Option<bool>,
    ) -> Result<Self, Error> {
        let bases = bases.unwrap_or(default_basis()?);
        let spline = SplineObject::new(bases, controlpoints, rational)?;

        Ok(Curve { spline })
    }

    /// Evaluate the object at given parametric values.
    ///
    /// ### parameters
    /// * t: collection of parametric coordinates in which to evaluate
    /// Realistically, this should actually be an Array1 object, but for
    /// consistency with the underlying SplineObject methods the collection
    /// is used instead.
    ///
    /// ### returns
    /// * 2D array
    // pub fn evaluate(&self, t: &mut &[Array1<f64>], tensor: bool) -> Result<ArrayD<f64>, Error> {
    pub fn evaluate(&self, t: &mut &[Array1<f64>]) -> Result<Array2<f64>, Error> {
        self.spline.validate_domain(t)?;

        let mut tx = t[0].clone().to_owned();
        let n_csr = self.spline.bases[0].evaluate(&mut tx, 0, true)?;

        // kludge...
        let n0 = self.spline.controlpoints.shape()[0];
        let n1 = self.spline.controlpoints.shape()[1];
        let flat_controlpoints = self
            .ravel(&self.spline.controlpoints)
            .into_raw_vec()
            .to_vec();

        let arr = Array2::<f64>::from_shape_vec((n0, n1), flat_controlpoints)?;
        let mut result = n_csr.todense().dot(&arr);

        // if the spline is rational, we apply the weights and omit the weights column
        if self.spline.rational {
            let wpos = result.shape()[1] - 1;
            let weights = &&result.slice(s![.., wpos]);
            let mut temp = Array2::<f64>::zeros((result.shape()[0], wpos));

            for i in 0..self.spline.dimension {
                {
                    let mut slice = temp.slice_mut(s![.., i]);
                    let update = result.slice(s![.., i]).to_owned() / weights.to_owned();
                    slice.assign(&update.view());
                }
            }
            result = temp;
        }

        Ok(result)
    }

    /// Extend the curve by merging another curve to the end of it.
    ///
    /// The curves are glued together in a C0 fashion with enough repeated
    /// knots. The function assumes that the end of this curve perfectly
    /// matches the start of the input curve.
    ///
    /// Obviously, neither curve can be periodic, which enables us
    /// to assume all knot vectors are open.
    ///
    /// ### parameters
    /// * curve: Another curve
    ///
    /// ### returns
    pub fn append(&mut self, othercurve: Curve) -> Result<(), Error> {
        if self.spline.bases[0].periodic > -1 || othercurve.spline.bases[0].periodic > -1 {
            return Err(Error::CannotConnectPeriodicCurves);
        };

        let mut extending_curve = othercurve;

        // make sure both are in the same space, and (if needed) have rational weights
        self.spline
            .make_splines_compatible(&mut extending_curve.spline)?;
        let p1 = self.spline.order(0)?[0];
        let p2 = extending_curve.spline.order(0)?[0];

        if p1 < p2 {
            self.raise_order(p2 - p1)?;
        } else {
            extending_curve.raise_order(p1 - p2)?;
        }

        let p = max(p1, p2);

        let old_knot = self.spline.knots(0, Some(true))[0].clone();
        let mut add_knot = extending_curve.spline.knots(0, Some(true))[0].clone();
        add_knot -= add_knot[0];
        add_knot += old_knot[old_knot.len() - 1];

        let mut new_knot = Array1::<f64>::zeros(add_knot.len() + old_knot.len() - p - 1);
        {
            let mut slice = new_knot.slice_mut(s![..old_knot.len() - 1]);
            let update = old_knot.slice(s![..old_knot.len() - 1]);
            slice.assign(&update.view());
        }
        {
            let mut slice = new_knot.slice_mut(s![old_knot.len() - 1..]);
            let update = add_knot.slice(s![p..]);
            slice.assign(&update.view());
        }

        let rational = self.spline.rational as usize;
        let n1 = self.spline.controlpoints.shape()[0];
        let n2 = extending_curve.spline.controlpoints.shape()[0];
        let n3 = self.spline.dimension + rational;
        let mut new_controlpoints = Array2::<f64>::zeros((n1 + n2 - 1, n3));
        {
            let mut slice = new_controlpoints.slice_mut(s![..n1, ..]);
            let update = self.spline.controlpoints.slice(s![.., ..]);
            slice.assign(&update.view());
        }
        {
            let mut slice = new_controlpoints.slice_mut(s![n1.., ..]);
            let update = extending_curve.spline.controlpoints.slice(s![1.., ..]);
            slice.assign(&update.view());
        }

        self.spline.bases = vec![BSplineBasis::new(Some(p), Some(new_knot), None)?];
        self.spline.controlpoints = new_controlpoints.into_dyn().to_owned();

        Ok(())
    }

    /// Raise the polynomial order of the curve.
    ///
    /// ### parameters
    /// * amount: Number of times to raise the order
    pub fn raise_order(&mut self, amount: usize) -> Result<(), Error> {
        if amount == 0 {
            return Ok(());
        }

        // work outside of self, copy back in at the end
        let mut new_basis = self.spline.bases[0].clone();
        new_basis.raise_order(amount);

        // set up an interpolation problem. This is in projective space,
        // so no problems for rational cases
        let mut interpolation_pts_t = new_basis.greville();
        let n_old = self.spline.bases[0].evaluate(&mut interpolation_pts_t, 0, true)?;
        let n_new = new_basis.evaluate(&mut interpolation_pts_t, 0, true)?;

        // Some kludge required to enable .dot(), which doesn't work on dynamic
        // arrays. Surely a better way to do this, but this is quick and dirty
        // and valid since we're in curve land
        let n0 = self.spline.controlpoints.shape()[0];
        let n1 = self.spline.controlpoints.shape()[1];
        let flat_controlpoints = self
            .ravel(&self.spline.controlpoints)
            .into_raw_vec()
            .to_vec();

        let arr = Array2::<f64>::from_shape_vec((n0, n1), flat_controlpoints)?;
        let interpolation_pts_x = n_old.todense().dot(&arr);
        let res = n_new.matrix_solve(&interpolation_pts_x)?;

        self.spline.controlpoints = res.into_dyn().to_owned();
        self.spline.bases = vec![new_basis];

        Ok(())
    }

    /// Computes the euclidian length of the curve in geometric space
    ///
    /// .. math:: \\int_{t_0}^{t_1}\\sqrt{x(t)^2 + y(t)^2 + z(t)^2} dt
    ///
    /// ### parameters
    /// * t0: lower integration limit
    /// * t1: upper integration limit
    pub fn length(&self, t0: Option<f64>, t1: Option<f64>) -> Result<f64, Error> {
        let mut knots = &self.spline.knots(0, Some(false))[0];

        // keep only integration boundaries within given start (t0) and stop (t1) interval
        let new_knots_0 = t0
            .map(|t0| {
                let i = bisect_left(knots, &t0, knots.len());
                let mut vec = Vec::<f64>::with_capacity(&knots.to_vec()[i..].len() + 1);
                vec.push(t0);
                for elem in knots.to_vec()[i..].iter() {
                    vec.push(*elem);
                }
                Array1::<f64>::from_vec(vec)
            })
            .unwrap_or_else(|| knots.to_owned());
        knots = &new_knots_0;

        let new_knots_1 = t1
            .map(|t1| {
                let i = bisect_right(knots, &t1, knots.len());
                let mut vec = Vec::<f64>::with_capacity(&knots.to_vec()[..i].len() + 1);
                for elem in knots.to_vec()[..i].iter() {
                    vec.push(*elem);
                }
                vec.push(t1);
                Array1::<f64>::from_vec(vec)
            })
            .unwrap_or_else(|| knots.to_owned());
        knots = &new_knots_1;

        let klen = knots.len();
        let gleg = GaussLegendreQuadrature::new(self.spline.order(0)?[0] + 1)?;

        let t = &knots.to_vec()[..klen - 1]
            .iter()
            .zip(knots.to_vec()[1..].iter())
            .map(|(t0, t1)| (&gleg.sample_points + 1.) / 2. * (t1 - t0) + *t0)
            .collect::<Vec<_>>();

        let w = &knots.to_vec()[..klen - 1]
            .iter()
            .zip(knots.to_vec()[1..].iter())
            .map(|(t0, t1)| &gleg.weights / 2. * (t1 - t0))
            .collect::<Vec<_>>();

        let t_flat = self.flattened(&t[..]);
        let w_flat = self.flattened(&w[..]);

        let dx = self.derivative(&t_flat, 1, Some(true))?;
        let det_j = Array1::<f64>::from_vec(
            dx.mapv(|e| e.powi(2))
                .sum_axis(Axis(1))
                .mapv(f64::sqrt)
                .iter()
                .copied()
                .collect::<Vec<_>>(),
        );
        let out = det_j.dot(&w_flat);

        Ok(out)
    }

    fn flattened(&self, vec_arr: &[Array1<f64>]) -> Array1<f64> {
        let alloc = vec_arr.iter().fold(0, |sum, e| sum + e.len());
        let mut vec_out = Vec::<f64>::with_capacity(alloc);
        for arr in vec_arr.iter() {
            for e in arr.to_vec().iter() {
                vec_out.push(*e);
            }
        }

        Array1::<f64>::from_vec(vec_out)
    }

    /// left here as a private method as it assumes C-contiguous ordering,
    /// which is fine for where we use it here.
    fn ravel(&self, arr: &ArrayD<f64>) -> Array1<f64> {
        let alloc = arr.shape().iter().product();
        let mut vec = Vec::<f64>::with_capacity(alloc);
        for e in arr.iter() {
            vec.push(*e)
        }

        Array1::<f64>::from_vec(vec)
    }

    /// Evaluate the derivative of the curve at the given parametric values.
    ///
    /// This function returns an *n* × *dim* array, where *n* is the number of
    /// evaluation points, and *dim* is the physical dimension of the curve.
    /// **At this point in time, only `dim == 1` works, owing to the provisional
    /// constraints on the struct `Curve` itself.**
    ///
    /// ### parameters
    /// * t: Parametric coordinates in which to evaluate
    /// * d: Number of derivatives to compute
    /// * from_right: Evaluation in the limit from right side
    pub fn derivative(
        &self,
        t: &Array1<f64>,
        d: usize,
        from_right: Option<bool>,
    ) -> Result<ArrayD<f64>, Error> {
        let from_right = from_right.unwrap_or(true);

        if !self.spline.rational || d < 2 || d > 3 {
            let mut tx = &vec![t.clone().to_owned()][..];
            let res = self.spline.derivative(&mut tx, &[d], &[from_right], true)?;
            return Ok(res);
        }

        // init rusult array
        let mut res = Array2::<f64>::zeros((t.len(), self.spline.dimension));

        // annoying fix to make the controlpoints not dynamic--implicit
        // assumption of 2D curve only!
        let n0 = self.spline.controlpoints.shape()[0];
        let n1 = self.spline.controlpoints.shape()[1];
        let flat_controlpoints = self
            .ravel(&self.spline.controlpoints)
            .into_raw_vec()
            .to_vec();
        let static_controlpoints = Array2::<f64>::from_shape_vec((n0, n1), flat_controlpoints)?;
        let mut t_eval = t.clone();
        let d2 = self.spline.bases[0]
            .evaluate(&mut t_eval, 2, from_right)?
            .todense()
            .dot(&static_controlpoints);
        let d1 = self.spline.bases[0]
            .evaluate(&mut t_eval, 1, from_right)?
            .todense()
            .dot(&static_controlpoints);
        let d0 = self.spline.bases[0]
            .evaluate(&mut t_eval, 0, true)?
            .todense()
            .dot(&static_controlpoints);
        let w0 = &d0.slice(s![.., d0.shape()[1] - 1]).to_owned();
        let w1 = &d1.slice(s![.., d1.shape()[1] - 1]).to_owned();
        let w2 = &d2.slice(s![.., d2.shape()[1] - 1]).to_owned();

        if d == 2 {
            let w0_cube = &w0.mapv(|e| e.powi(3)).to_owned();
            for i in 0..self.spline.dimension {
                {
                    let update = &((d2.slice(s![.., i]).to_owned() * w0 * w0
                        - 2. * w1
                            * (d1.slice(s![.., i]).to_owned() * w0
                                - d0.slice(s![.., i]).to_owned() * w1)
                        - d0.slice(s![.., i]).to_owned() * w2 * w1)
                        / w0_cube);

                    let mut slice = res.slice_mut(s![.., i]);
                    slice.assign(update);
                }
            }
        }

        if d == 3 {
            let d3 = self.spline.bases[0]
                .evaluate(&mut t_eval, 3, from_right)?
                .todense()
                .dot(&static_controlpoints);
            let w3 = &d3.slice(s![.., d3.shape()[1] - 1]);
            let w0_four = w0.mapv(|e| e.powi(6));
            for i in 0..self.spline.dimension {
                {
                    let h0 = &(d1.slice(s![.., i]).to_owned() * w0
                        - d0.slice(s![.., i]).to_owned() * w1);
                    let h1 = &(d2.slice(s![.., i]).to_owned() * w0
                        - d0.slice(s![.., i]).to_owned() * w2);
                    let h2 = &(d3.slice(s![.., i]).to_owned() * w0
                        + d2.slice(s![.., i]).to_owned() * w1
                        - d1.slice(s![.., i]).to_owned() * w2
                        - d0.slice(s![.., i]).to_owned() * w3);
                    let g0 = &(h1 * w0 - 2. * h0 * w1);
                    let g1 = &(h2 * w0 - 2. * h0 * w2 - h1 * w1);

                    let update = (g1 * w0 - 3. * g0 * w1) / &w0_four;
                    let mut slice = res.slice_mut(s![.., i]);
                    slice.assign(&update);
                }
            }
        }

        Ok(res.into_dyn().to_owned())
    }

    /// Computes the L2 (squared and per knot span) between this
    /// curve and a target curve as well as the L_infinity error:
    ///
    /// .. math:: ||\\boldsymbol{x_h}(t)-\\boldsymbol{x}(t)||_{L^2(t_1,t_2)}^2 = \\int_{t_1}^{t_2}
    ///     |\\boldsymbol{x_h}(t)-\\boldsymbol{x}(t)|^2 dt, \\quad \\forall \\;\\text{knots}\\;t_1 <
    /// t_2
    ///
    /// .. math:: ||\\boldsymbol{x_h}(t)-\\boldsymbol{x}(t)||_{L^\\infty} = \\max_t
    /// |\\boldsymbol{x_h}(t)-\\boldsymbol{x}(t)|
    ///
    /// ### parameters
    /// * function target: callable function which takes as input a vector
    /// of evaluation points t and gives as output a matrix x where
    /// x[i,j] is component j evaluated at point t[i]
    ///
    /// ### returns
    /// * L2 error per knot-span
    pub fn error(
        &self,
        target: impl Fn(&Array1<f64>) -> Array2<f64>,
    ) -> Result<(Array1<f64>, f64), Error> {
        let knots = &self.spline.knots(0, Some(false))[0];
        let n = self.spline.order(0)?[0];
        let gleg = GaussLegendreQuadrature::new(n + 1)?;

        let mut error_l2 = Vec::with_capacity(knots.len() - 1);
        let mut error_linf = Vec::with_capacity(knots.len() - 1);

        for (t0, t1) in knots.to_vec()[..knots.len() - 1]
            .iter()
            .zip(&mut knots.to_vec()[1..].iter())
        {
            let tg = (&gleg.sample_points + 1.) / 2. * (t1 - t0) + *t0;
            let eval = vec![tg.clone()];
            let wg = &gleg.weights / 2. * (t1 - t0);

            let exact = target(&tg);
            let error = self.evaluate(&mut &eval[..])? - exact;
            let error_2 = &error.mapv(|e| e.powi(2)).sum_axis(Axis(1));
            let error_abs = &error_2.mapv(|e| e.sqrt());

            let l2_val = error_2.dot(&wg);
            let linf_val = error_abs.iter().copied().fold(f64::NEG_INFINITY, f64::max);

            error_l2.push(l2_val);
            error_linf.push(linf_val);
        }

        let out_inf = error_linf.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        Ok((Array1::<f64>::from_vec(error_l2), out_inf))
    }
}
