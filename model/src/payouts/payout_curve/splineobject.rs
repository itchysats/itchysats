use crate::payouts::payout_curve::basis::BSplineBasis;
use crate::payouts::payout_curve::csr_tools::CSR;
use crate::payouts::payout_curve::Error;
use itertools::Itertools;
use ndarray::concatenate;
use ndarray::prelude::*;
use ndarray::Order;
use ndarray_einsum_beta::einsum;
use ndarray_einsum_beta::tensordot;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct SplineObject {
    pub bases: Vec<BSplineBasis>,
    pub controlpoints: ArrayD<f64>,
    pub dimension: usize,
    pub rational: bool,
    pub pardim: usize,
}

/// Master struct for spline objects with arbitrary dimensions.
///
/// This class should be composed instead of used directly.
impl SplineObject {
    pub fn new(
        bases: Vec<BSplineBasis>,
        controlpoints: Option<Array2<f64>>,
        rational: Option<bool>,
    ) -> Result<Self, Error> {
        let mut controlpoints = match controlpoints {
            Some(controlpoints) => controlpoints,
            None => default_control_points(&bases)?,
        };
        let rational = rational.unwrap_or(false);

        if controlpoints.slice(s![0, ..]).shape()[0] == 1 {
            controlpoints = concatenate(
                Axis(1),
                &[
                    controlpoints.view(),
                    Array1::<f64>::zeros(controlpoints.shape()[0])
                        .insert_axis(Axis(1))
                        .view(),
                ],
            )?;
        }

        if rational {
            controlpoints = concatenate(
                Axis(1),
                &[
                    controlpoints.view(),
                    Array1::<f64>::ones(controlpoints.shape()[0])
                        .insert_axis(Axis(1))
                        .view(),
                ],
            )?;
        }

        let dim = controlpoints.shape()[1] - (rational as usize);
        let bases_shape = determine_shape(&bases)?;
        let ncomps = dim + (rational as usize);
        let cpts_shaped = reshaper(controlpoints, bases_shape, ncomps)?;
        let pardim = cpts_shaped.shape().len() - 1;

        Ok(SplineObject {
            bases,
            controlpoints: cpts_shaped,
            dimension: dim,
            rational,
            pardim,
        })
    }

    /// Check whether the given evaluation parameters are valid
    pub fn validate_domain(&self, t: &[Array1<f64>]) -> Result<(), Error> {
        for (basis, params) in self.bases.iter().zip(t.to_owned().iter_mut()) {
            if basis.periodic < 0 {
                basis.snap(&mut *params);
                let p_max = &params.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let p_min = &params.iter().copied().fold(f64::INFINITY, f64::min);
                if *p_min < basis.start() || basis.end() < *p_max {
                    return Err(Error::InvalidDomain);
                }
            }
        }

        Ok(())
    }

    fn tensor_evaluate(&self, eval_bases: &mut [CSR], tensor: bool) -> Result<ArrayD<f64>, Error> {
        // KLUDGE!
        // owing to the fact that the conventional ellipsis notation is not yet
        // implemented for einsum, we use this workaround that should cover us.
        // If not, just grow the maps as needed or address the issue:
        // https://github.com/oracleofnj/einsum/issues/6
        let init_map: HashMap<usize, &str> = [
            (2, "ij,jp->ip"),
            (3, "ij,jpq->ipq"),
            (4, "ij,jpqr->ipqr"),
            (5, "ij,jpqrs->ipqrs"),
            (6, "ij,jpqrst->ipqrst"),
        ]
        .iter()
        .cloned()
        .collect();

        let iter_map: HashMap<usize, &str> = [
            (3, "ij,ijp->ip"),
            (4, "ij,ijpq->ipq"),
            (5, "ij,ijpqr->ipqr"),
            (6, "ij,ijpqrs->ipqrs"),
        ]
        .iter()
        .cloned()
        .collect();

        let mut out;
        if tensor {
            eval_bases.reverse();
            let cpts = self.controlpoints.clone().to_owned();
            let idx = eval_bases.len() - 1;

            out = eval_bases.iter().fold(cpts, |e, tns| {
                tensordot(&tns.todense(), &e, &[Axis(1)], &[Axis(idx)])
            });
        } else {
            let mut pos = 0;
            let mut key = self.bases.len() + 1;
            let mut val = match init_map.get(&key) {
                Some(val) => Ok(val),
                _ => Err(Error::NoEinsumOperatorString),
            }?;

            out = einsum(val, &[&eval_bases[pos].todense(), &self.controlpoints])
                .map_err(|_| Error::Einsum)?;

            for _ in eval_bases.iter().skip(1) {
                pos += 1;
                val = match iter_map.get(&key) {
                    Some(val) => Ok(val),
                    _ => Err(Error::NoEinsumOperatorString),
                }?;
                let temp = out.clone().to_owned();

                out =
                    einsum(val, &[&eval_bases[pos].todense(), &temp]).map_err(|_| Error::Einsum)?;

                key -= 1;
            }
        }
        // *** END KLUDGE ****

        Ok(out)
    }

    /// Evaluate the derivative of the object at the given parametric values.
    ///
    /// If *tensor* is true, evaluation will take place on a tensor product
    /// grid, i.e. it will return an *n1* × *n2* × ... × *dim* array, where
    /// *ni* is the number of evaluation points in direction *i*, and *dim* is
    /// the physical dimension of the object.
    ///
    /// If *tensor* is false, there must be an equal number *n* of evaluation
    /// points in all directions, and the return value will be an *n* × *dim*
    /// array.
    ///
    /// ### parameters
    /// * t = [u,v,...]: Parametric coordinates in which to evaluate
    /// * d: Order of derivative to compute, index corresponds to bases index
    /// * from_right: Evaluation in the limit from above; index orresponds to bases index
    /// * tensor: Whether to evaluate on a tensor product grid
    pub fn derivative(
        &self,
        t: &mut &[Array1<f64>],
        d: &[usize],
        from_right: &[bool],
        tensor: bool,
    ) -> Result<ArrayD<f64>, Error> {
        // check
        let testlen = t.len();
        let ops = [t.len(), d.len(), from_right.len()]
            .iter()
            .all(|e| *e == testlen);
        if !tensor && !ops {
            return Err(Error::InvalidDerivative);
        }

        self.validate_domain(t)?;

        // Evaluate the derivatives of the corresponding bases at the corresponding points
        // and build the result array
        let mut evals = self
            .bases
            .iter()
            .zip(t.iter().zip(d.iter().zip(from_right.iter())))
            .map(|(b, (t, (d, r)))| {
                let mut tx = t.clone();
                let eval = b.evaluate(&mut tx, *d, *r)?;
                Ok(eval)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let mut result = self.tensor_evaluate(&mut evals, tensor)?;

        // For rational curves, we need to use the quotient rule
        // (n/W)' = (n' W - n W') / W^2 = n'/W - nW'/W^2
        //   n'(i) = result[..., i]
        //   W'(i) = result[..., -1]
        if self.rational {
            if d.iter().sum::<usize>() > 1 {
                return Err(Error::DerivativeNotImplemented);
            }

            let mut ns = self
                .bases
                .iter()
                .zip(t.iter())
                .map(|(b, t)| {
                    let mut tx = t.clone();
                    let eval = b.evaluate(&mut tx, 0, true)?;
                    Ok(eval)
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let non_derivative = self.tensor_evaluate(&mut ns, tensor)?;

            let axis_w = non_derivative.shape().len() - 1;
            let idx_w = non_derivative.shape()[axis_w] - 1;
            let w = &non_derivative.index_axis(Axis(axis_w), idx_w).to_owned();
            let w_square = &w.mapv(|e| e.powi(2)).to_owned();

            let axis_r = result.shape().len() - 1;
            let idx_r = result.shape()[axis_r] - 1;
            let wd = &result.index_axis(Axis(axis_r), idx_r).to_owned();

            for i in 0..self.dimension {
                {
                    let update = &(result.index_axis(Axis(axis_r), i).to_owned() / w
                        - non_derivative.index_axis(Axis(axis_w), i).to_owned() * wd / w_square);
                    let mut slice = result.index_axis_mut(Axis(axis_r), i);
                    slice.assign(update);
                }
            }

            // delete the last column; some faffing about required to maintain
            // C-contiguous ordering. Probably a much better way to do this...
            let res_shape = &result.shape().to_vec();
            let mut n_res: usize = res_shape[..axis_r].iter().product();
            n_res *= idx_r;
            let idx = (0..axis_r).collect::<Vec<_>>();

            // this ends up being F-contiguous, every time
            let res_slice = result.select(Axis(axis_r), &idx[..]).to_owned();
            let raveled = res_slice.to_shape(((n_res,), Order::C))?;
            let fixed = raveled.to_shape((res_slice.shape(), Order::C))?.to_owned();

            result = fixed;
        }

        Ok(result)
    }

    /// Return knots vector
    ///
    /// If `direction` is given, returns the knots in that direction only.
    /// Otherwise, specifying direction as a negative value  returns the
    /// knots of all directions.
    ///
    /// ### parameters
    /// * direction: Direction number (axis) in which to get the knots.
    /// * with_multiplicities: If true, return knots with multiplicities \
    /// (i.e. repeated).
    pub fn knots(&self, direction: isize, with_multiplicities: Option<bool>) -> Vec<Array1<f64>> {
        let with_multiplicities = with_multiplicities.unwrap_or(false);
        let out;

        if direction < 0 {
            if with_multiplicities {
                out = self
                    .bases
                    .iter()
                    .map(|e| e.knots.clone().to_owned())
                    .collect::<Vec<_>>();
            } else {
                out = self
                    .bases
                    .iter()
                    .map(|e| e.knot_spans(false).to_owned())
                    .collect::<Vec<_>>();
            }
        } else {
            let p = direction as usize;
            if with_multiplicities {
                out = (&[self.bases[p].knots.clone().to_owned()]).to_vec();
            } else {
                out = (&[self.bases[p].knot_spans(false).to_owned()]).to_vec();
            }
        }

        out
    }

    /// This will manipulate one or both to ensure that they are both rational
    /// or nonrational, and that they lie in the same physical space.
    pub fn make_splines_compatible(&mut self, otherspline: &mut SplineObject) -> Result<(), Error> {
        if self.rational {
            otherspline.force_rational()?;
        } else if otherspline.rational {
            self.force_rational()?;
        }

        if self.dimension > otherspline.dimension {
            otherspline.set_dimension(self.dimension)?;
        } else {
            self.set_dimension(otherspline.dimension)?;
        }

        Ok(())
    }

    /// Force a rational representation of the object.
    pub fn force_rational(&mut self) -> Result<(), Error> {
        if !self.rational {
            self.controlpoints = self.insert_phys(&self.controlpoints, 1f64)?;
            self.rational = true;
        }

        Ok(())
    }

    /// Sets the physical dimension of the object. If increased, the new
    /// components are set to zero.
    ///
    /// ### parameters
    /// * new_dim: New dimension
    pub fn set_dimension(&mut self, new_dim: usize) -> Result<(), Error> {
        let mut dim = self.dimension;

        while new_dim > dim {
            self.controlpoints = self.insert_phys(&self.controlpoints, 0f64)?;
            dim += 1;
        }

        while new_dim < dim {
            let axis = if self.rational { -2 } else { -1 };
            self.controlpoints = self.delete_phys(&self.controlpoints, axis)?;
            dim -= 1;
        }

        self.dimension = new_dim;

        Ok(())
    }

    fn insert_phys(&self, arr: &ArrayD<f64>, insert_value: f64) -> Result<ArrayD<f64>, Error> {
        let mut arr_shape = arr.shape().to_vec();
        let n = arr_shape[arr_shape.len() - 1];
        let arr_prod = arr_shape.iter().product();

        let raveled = arr.to_shape(((arr_prod,), Order::C))?;
        let mut new_arr = Array1::<f64>::zeros(0);

        for i in (0..raveled.len()).step_by(n) {
            let new_row = concatenate(
                Axis(0),
                &[
                    raveled.slice(s![i..i + n]).view(),
                    (insert_value * Array1::<f64>::ones(1)).view(),
                ],
            )?;
            new_arr = concatenate(Axis(0), &[new_arr.view(), new_row.view()])?;
        }

        arr_shape[n] += 1;
        let out = new_arr.to_shape((&arr_shape[..], Order::C))?.to_owned();

        Ok(out)
    }

    fn delete_phys(&self, arr: &ArrayD<f64>, axis: isize) -> Result<ArrayD<f64>, Error> {
        let mut arr_shape = arr.shape().to_vec();
        let n = arr_shape[arr_shape.len() - 1];
        let step = (n as isize + axis) as usize;
        let arr_prod = arr_shape.iter().product();

        let raveled = arr.to_shape(((arr_prod,), Order::C))?;
        let mut new_arr = Array1::<f64>::zeros(0);

        for i in (0..raveled.len()).step_by(n) {
            let new_row = if axis < -1 {
                let front = raveled.slice(s![i..i + step]).clone().to_owned();
                let tail = raveled.slice(s![i + step + 1..i + n]).clone().to_owned();
                concatenate(Axis(0), &[front.view(), tail.view()])?
            } else {
                raveled.slice(s![i..i + step]).clone().to_owned()
            };
            new_arr = concatenate(Axis(0), &[new_arr.view(), new_row.view()])?;
        }

        arr_shape[n - 1] -= 1;
        let out = new_arr.to_shape((&arr_shape[..], Order::C))?.to_owned();

        Ok(out)
    }

    /// Return polynomial order (degree + 1).
    ///
    /// If `direction` is given, returns the order in that direction only.
    /// Otherwise, specifying direction as a negative value  returns the
    /// order of all directions.
    ///
    /// ### parameters
    ///  * direction: Direction in which to get the order.
    pub fn order(&self, direction: isize) -> Result<Vec<usize>, Error> {
        let out = if direction < 0 {
            self.bases.iter().map(|e| e.order).collect::<Vec<_>>()
        } else {
            let p = direction as usize;
            (&[self.bases[p].order]).to_vec()
        };

        Ok(out)
    }
}

fn default_control_points(bases: &[BSplineBasis]) -> Result<Array2<f64>, Error> {
    let mut temp = bases
        .iter()
        .rev()
        .map(|b| {
            let mut v = b.greville().into_raw_vec();
            v.reverse();
            v
        })
        .multi_cartesian_product()
        .collect::<Vec<_>>();
    temp.reverse();

    // because the above is just a little bit incorrect...
    for elem in temp.iter_mut() {
        elem.reverse();
    }

    let mut data = Vec::new();
    let ncols = temp.first().map_or(0, |row| row.len());
    let mut nrows = 0;

    for elem in temp.iter() {
        data.extend_from_slice(elem);
        nrows += 1;
    }

    let out = Array2::from_shape_vec((nrows, ncols), data)?;

    Ok(out)
}

/// Custom reshaping function to preserve control points of several
/// dimensions that are stored contiguously.
///
/// The return value has shape (*newshape, ncomps), where ncomps is
/// the number of components per control point, as inferred by the
/// size of `arr` and the desired shape.
fn reshaper(
    arr: Array2<f64>,
    mut newshape: Vec<usize>,
    ncomps: usize,
) -> Result<ArrayD<f64>, Error> {
    newshape.reverse();
    newshape.push(ncomps);

    let mut spec: Vec<usize> = (0..newshape.len() - 1).collect();
    spec.reverse();
    spec.push(newshape.len() - 1);

    let tmp = arr.to_shape((&newshape[..], Order::C))?;
    let tmp = tmp.to_owned().into_dyn();

    let out = tmp.view().permuted_axes(&spec[..]).to_owned();

    Ok(out)
}

fn determine_shape(bases: &[BSplineBasis]) -> Result<Vec<usize>, Error> {
    let out = bases
        .iter()
        .map(|e| e.num_functions())
        .collect::<Vec<usize>>();

    Ok(out)
}
