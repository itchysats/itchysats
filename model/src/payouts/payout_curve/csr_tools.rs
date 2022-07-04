use crate::payouts::payout_curve::compat::To1DArray;
use crate::payouts::payout_curve::compat::ToNAlgebraMatrix;
use crate::payouts::payout_curve::Error;
use itertools::Itertools;
use ndarray::prelude::*;
use std::ops::Mul;

/// NOTE:
/// This struct is provided here in this form as nalgebra_sparse
/// is rather embryonic and incluldes (basically) no solvers
/// at present. As we only need to be able construct as CSR
/// matrix and perform multiplication with a (dense) vector, it
/// seemed to make more sense to define our own rudementary CSR struct
/// and avoid the bloat of using a crate that will introduce
/// breaking changes regularly, and we need to write our own
/// solver regardless.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
pub struct CSR {
    pub data: Array1<f64>,
    pub indices: Array1<usize>,
    pub indptr: Array1<usize>,
    pub shape: (usize, usize),
    pub nnz: usize,
}

impl CSR {
    pub fn new(
        data: Array1<f64>,
        indices: Array1<usize>,
        indptr: Array1<usize>,
        shape: (usize, usize),
    ) -> Result<Self, Error> {
        let major_dim: isize = (indptr.len() as isize) - 1;
        let nnz = &data.len();

        if major_dim > 1 && shape.0 as isize == major_dim {
            Ok(CSR {
                data,
                indices,
                indptr,
                shape,
                nnz: *nnz,
            })
        } else {
            Err(Error::CannotInitCSR)
        }
    }

    // matrix version of `solve()`; useful for solving AX = B. Implementation
    // is horrible.
    pub fn matrix_solve(&self, b_arr: &Array2<f64>) -> Result<Array2<f64>, Error> {
        let a_arr = self.todense();
        let zeros = Array2::zeros((b_arr.nrows(), 0));

        let result = b_arr
            .columns()
            .into_iter()
            .map(|b| lu_solve(&a_arr, &b.to_owned()))
            .fold_ok(zeros, |mut result, column| {
                result
                    .push_column(column.view())
                    .expect("shape was initialized correctly");

                result
            })?;

        Ok(result)
    }

    pub fn todense(&self) -> Array2<f64> {
        let mut out = Array2::<f64>::zeros(self.shape);
        for i in 0..self.shape.0 {
            for j in self.indptr[i]..self.indptr[i + 1] {
                out[[i, self.indices[j]]] += self.data[j];
            }
        }

        out
    }
}

fn lu_solve(a: &Array2<f64>, b: &Array1<f64>) -> Result<Array1<f64>, Error> {
    if !is_square(a) {
        return Err(Error::MatrixMustBeSquare);
    }
    let a = a.to_nalgebra_matrix().lu();
    let b = b.to_nalgebra_matrix();

    let x = a.solve(&b).ok_or(Error::SingularMatrix)?.to_1d_array()?;

    Ok(x)
}

fn is_square<T>(arr: &Array2<T>) -> bool {
    arr.shape()[0] == arr.shape()[1]
}

impl Mul<&Array1<f64>> for CSR {
    type Output = Array1<f64>;

    fn mul(self, rhs: &Array1<f64>) -> Array1<f64> {
        let mut out = Array1::<f64>::zeros(self.shape.0);
        for i in 0..self.shape.0 {
            for j in self.indptr[i]..self.indptr[i + 1] {
                out[i] += self.data[j] * rhs[self.indices[j]];
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lu_solve() {
        let a = array![[11., 12., 0.], [0., 22., 23.], [31., 0., 33.]];
        let b = array![35., 113., 130.];
        let x_expected = array![1., 2., 3.];

        let x = lu_solve(&a, &b).unwrap();

        assert!(x.abs_diff_eq(&x_expected, 1e-10));
    }

    #[test]
    fn negative_csr_test_00() {
        let a = CSR::new(
            Array1::zeros(0),
            Array1::zeros(0),
            Array1::zeros(11),
            (1, 3),
        )
        .unwrap_err();

        assert!(matches!(a, Error::CannotInitCSR));
    }

    // test that all is good
    #[test]
    fn test_lu_matrix_solve_00() {
        let a = CSR::new(
            array![11., 12., 22., 23., 31., 33.],
            array![0, 1, 1, 2, 0, 2],
            array![0, 2, 4, 6],
            (3, 3),
        )
        .unwrap();
        let b = array![[36., 59., 82.], [204., 249., 294.], [198., 262., 326.],];

        let x_expected = array![[0., 1., 2.], [3., 4., 5.], [6., 7., 8.]];
        let x = a.matrix_solve(&b).unwrap();

        assert!(x.abs_diff_eq(&x_expected, 1e-10));
    }

    // test that an indeterminate system borks
    #[test]
    fn test_lu_matrix_solve_01() {
        let a = CSR::new(array![1.], array![2], array![0, 1, 1, 1], (3, 3)).unwrap();
        let b = array![[36., 59., 82.], [204., 249., 294.], [198., 262., 326.],];

        let e = a.matrix_solve(&b).unwrap_err();

        assert!(matches!(e, Error::SingularMatrix));
    }

    // test that an incompatible system borks
    #[test]
    fn test_lu_matrix_solve_02() {
        let a = CSR::new(
            array![11., 12., 14., 19., 22., 23., 31., 33.],
            array![0, 1, 3, 5, 1, 2, 3, 5],
            array![0, 4, 8],
            (2, 6),
        )
        .unwrap();
        let b = array![[447., 503., 559.], [978., 1087., 1196.]];

        let e = a.matrix_solve(&b).unwrap_err();

        assert!(matches!(e, Error::MatrixMustBeSquare));
    }
}
