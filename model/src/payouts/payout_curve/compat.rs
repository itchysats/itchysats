use nalgebra::ComplexField;
use nalgebra::DMatrix;
use nalgebra::Dynamic;
use nalgebra::Scalar;
use ndarray::Array1;
use ndarray::Array2;
use std::fmt::Debug;

pub trait ToNAlgebraMatrix<T> {
    fn to_nalgebra_matrix(&self) -> DMatrix<T>;
}

pub trait To1DArray<T> {
    fn to_1d_array(&self) -> Result<Array1<T>, NotOneDimensional>;
}

pub trait To2DArray<T> {
    fn to_2d_array(&self) -> Array2<T>;
}

impl<T: Clone + ComplexField + Scalar + PartialEq + Debug + PartialEq> ToNAlgebraMatrix<T>
    for Array1<T>
{
    fn to_nalgebra_matrix(&self) -> DMatrix<T> {
        DMatrix::from_row_slice_generic(
            Dynamic::new(self.len()),
            Dynamic::new(1),
            self.to_vec().as_slice(),
        )
    }
}

impl<T: Clone + ComplexField + Scalar + PartialEq + Debug + PartialEq> ToNAlgebraMatrix<T>
    for Array2<T>
{
    fn to_nalgebra_matrix(&self) -> DMatrix<T> {
        let flattened = self.rows().into_iter().fold(Vec::new(), |mut acc, next| {
            acc.extend(next.into_iter().cloned());

            acc
        });

        DMatrix::from_row_slice_generic(
            Dynamic::new(self.nrows()),
            Dynamic::new(self.ncols()),
            flattened.as_slice(),
        )
    }
}

impl<T: Clone + PartialEq + Scalar> To1DArray<T> for DMatrix<T> {
    fn to_1d_array(&self) -> Result<Array1<T>, NotOneDimensional> {
        if self.ncols() != 1 {
            return Err(NotOneDimensional);
        }

        Ok(Array1::from_shape_fn(self.nrows(), |index| {
            self.get(index).unwrap().clone()
        }))
    }
}

impl<T: Clone + PartialEq + Scalar> To2DArray<T> for DMatrix<T> {
    fn to_2d_array(&self) -> Array2<T> {
        Array2::from_shape_fn((self.nrows(), self.ncols()), |indices| {
            self.get(indices).unwrap().clone()
        })
    }
}

#[derive(Debug, thiserror::Error, Clone, Copy)]
#[error("The provided matrix is not one-dimensional and cannot be converted into a 1D array")]
pub struct NotOneDimensional;
