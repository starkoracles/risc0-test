// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! An implementation of a 64-bit STARK-friendly prime field with modulus $2^{64} - 2^{32} + 1$
//! using Montgomery representation.
//! Our implementation follows https://eprint.iacr.org/2022/274.pdf and is constant-time.
//!
//! This field supports very fast modular arithmetic and has a number of other attractive
//! properties, including:
//! * Multiplication of two 32-bit values does not overflow field modulus.
//! * Field arithmetic in this field can be implemented using a few 32-bit addition, subtractions,
//!   and shifts.
//! * $8$ is the 64th root of unity which opens up potential for optimized FFT implementations.

use super::{ExtensibleField, FieldElement, StarkField};
use core::marker::PhantomData;
use core::{
    convert::{TryFrom, TryInto},
    fmt::{Debug, Display, Formatter},
    mem,
    ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign},
    slice,
};
use rkyv::{Archive, Deserialize as RD, Serialize as RS};
use serde::{Deserialize, Serialize};
use utils::{
    collections::Vec, string::ToString, AsBytes, ByteReader, ByteWriter, Deserializable,
    DeserializationError, Randomizable, Serializable,
};

#[cfg(any(feature = "generate-hints", feature = "use-hints"))]
pub mod hints {
    extern crate alloc;
    use alloc::collections::BTreeMap;
    use once_cell::sync::Lazy;
    use spin::Mutex;
    pub static INV_NONDET: Lazy<Mutex<BTreeMap<u64, u64>>> =
        Lazy::new(|| Mutex::new(BTreeMap::new()));

    pub static INV_NONDET_QUAD: Lazy<Mutex<BTreeMap<[u64; 2], [u64; 2]>>> =
        Lazy::new(|| Mutex::new(BTreeMap::new()));
}

#[cfg(any(feature = "generate-hints", feature = "use-hints"))]
pub use hints::{INV_NONDET, INV_NONDET_QUAD};

// CONSTANTS
// ================================================================================================

/// Field modulus = 2^64 - 2^32 + 1
const M: u64 = 0xFFFFFFFF00000001;

/// 2^128 mod M; this is used for conversion of elements into Montgomery representation.
const R2: u64 = 0xFFFFFFFE00000001;

/// 2^32 root of unity
const G: u64 = 1753635133440165772;

/// Number of bytes needed to represent field element
const ELEMENT_BYTES: usize = core::mem::size_of::<u64>();

pub trait NativeMontMul: Default + Debug + Copy + Sync + Send {
    // multiply two field elements in Montgomery representation, backed by u64
    fn native_mul_ext(a: [u64; 2], b: [u64; 2]) -> [u64; 2];
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultNativeMul {}
impl NativeMontMul for DefaultNativeMul {
    fn native_mul_ext(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
        let a_fp = [BaseElement::from_mont(a[0]), BaseElement::from_mont(a[1])];
        let b_fp = [BaseElement::from_mont(b[0]), BaseElement::from_mont(b[1])];

        let a0b0 = a_fp[0] * b_fp[0];
        let a1b1 = a_fp[1] * b_fp[1];
        let first = a0b0 - a1b1.double();
        let a0a1 = a_fp[0] + a_fp[1];
        let b0b1 = b_fp[0] + b_fp[1];
        let second = a0a1 * b0b1 - a0b0;

        [first.val, second.val]
    }
}

pub type BaseElement = AccelBaseElementRisc0<DefaultNativeMul>;

// FIELD ELEMENT
// ================================================================================================

/// Represents base field element in the field.
///
/// Internal values are stored in the range [0, 2^64). The backing type is `u64`.
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, Archive, RS, RD, PartialOrd, Ord)]
#[archive(compare(PartialEq))]
#[archive_attr(derive(Debug, PartialEq, PartialOrd, Eq, Ord))]
pub struct AccelBaseElementRisc0<A: NativeMontMul> {
    pub val: u64,
    #[omit_bounds]
    t: PhantomData<A>,
}
impl<A: NativeMontMul> AccelBaseElementRisc0<A> {
    /// Creates a new field element from the provided `value`; the value is converted into
    /// Montgomery representation.
    pub const fn convert_into(value: u64) -> AccelBaseElementRisc0<A> {
        Self {
            val: mont_red_cst((value as u128) * (R2 as u128)),
            t: PhantomData,
        }
    }

    /// Returns a new field element from the provided 'value'. Assumes that 'value' is already
    /// in canonical Montgomery form.
    pub const fn from_mont(value: u64) -> AccelBaseElementRisc0<A> {
        AccelBaseElementRisc0 {
            val: value,
            t: PhantomData,
        }
    }

    /// Returns the non-canonical u64 inner value.
    pub const fn inner(&self) -> u64 {
        self.val
    }

    /// Computes an exponentiation to the power 7. This is useful for computing Rescue-Prime
    /// S-Box over this field.
    #[inline(always)]
    pub fn exp7(self) -> Self {
        let x2 = self.square();
        let x4 = x2.square();
        let x3 = x2 * self;
        x3 * x4
    }
}

impl<A: NativeMontMul> FieldElement for AccelBaseElementRisc0<A> {
    type PositiveInteger = u64;
    type BaseField = Self;

    const ZERO: Self = Self::convert_into(0);
    const ONE: Self = Self::convert_into(1);

    const ELEMENT_BYTES: usize = ELEMENT_BYTES;
    const IS_CANONICAL: bool = false;

    #[inline]
    fn double(self) -> Self {
        let ret = (self.val as u128) << 1;
        let (result, over) = (ret as u64, (ret >> 64) as u64);
        Self::from_mont(result.wrapping_sub(M * (over as u64)))
    }

    #[inline]
    fn exp(self, power: Self::PositiveInteger) -> Self {
        // let mut b: Self;
        // let mut r = Self::ONE;
        // for i in (0..64).rev() {
        //     r = r.square();
        //     b = r;
        //     b *= self;
        //     // Constant-time branching
        //     let mask = -(((power >> i) & 1 == 1) as i64) as u64;
        //     r.0 ^= mask & (r.0 ^ b.0);
        // }

        // r
        // Special case for handling 0^0 = 1
        if power == 0 {
            return AccelBaseElementRisc0::ONE;
        }

        let mut acc = AccelBaseElementRisc0::ONE;
        let bit_length = 64 - power.leading_zeros();
        for i in 0..bit_length {
            acc = acc * acc;
            if power & (1 << (bit_length - 1 - i)) != 0 {
                acc *= self;
            }
        }

        acc
    }

    #[inline]
    #[allow(clippy::many_single_char_names)]
    fn inv(self) -> Self {
        #[cfg(feature = "use-hints")]
        {
            // means we are running as part of the verifier
            if let Some(res) = INV_NONDET.lock().get(&self.val) {
                let res_c = res.clone();
                assert!(Self::from_mont(res_c) * self == AccelBaseElementRisc0::ONE);
                return Self::from_mont(res_c);
            }
        }
        // compute base^(M - 2) using 72 multiplications
        // M - 2 = 0b1111111111111111111111111111111011111111111111111111111111111111

        // compute base^11
        let t2 = self.square() * self;

        // compute base^111
        let t3 = t2.square() * self;

        // compute base^111111 (6 ones)
        let t6 = exp_acc::<3, A>(t3, t3);

        // compute base^111111111111 (12 ones)
        let t12 = exp_acc::<6, A>(t6, t6);

        // compute base^111111111111111111111111 (24 ones)
        let t24 = exp_acc::<12, A>(t12, t12);

        // compute base^1111111111111111111111111111111 (31 ones)
        let t30 = exp_acc::<6, A>(t24, t6);
        let t31 = t30.square() * self;

        // compute base^111111111111111111111111111111101111111111111111111111111111111
        let t63 = exp_acc::<32, A>(t31, t31);

        // compute base^1111111111111111111111111111111011111111111111111111111111111111
        let res = t63.square() * self;
        #[cfg(all(feature = "generate-hints", feature = "std"))]
        {
            // means we are running as part of the prover
            INV_NONDET.lock().insert(self.val, res.val);
            // println!("inserted into INV_NONDET: {} => {}", self, res);
        }
        res
    }

    fn conjugate(&self) -> Self {
        Self::from_mont(self.val)
    }

    fn elements_as_bytes(elements: &[Self]) -> &[u8] {
        // TODO: take endianness into account.
        let p = elements.as_ptr();
        let len = elements.len() * Self::ELEMENT_BYTES;
        unsafe { slice::from_raw_parts(p as *const u8, len) }
    }

    unsafe fn bytes_as_elements(bytes: &[u8]) -> Result<&[Self], DeserializationError> {
        if bytes.len() % Self::ELEMENT_BYTES != 0 {
            return Err(DeserializationError::InvalidValue(format!(
                "number of bytes ({}) does not divide into whole number of field elements",
                bytes.len(),
            )));
        }

        let p = bytes.as_ptr();
        let len = bytes.len() / Self::ELEMENT_BYTES;

        if (p as usize) % mem::align_of::<u64>() != 0 {
            return Err(DeserializationError::InvalidValue(
                "slice memory alignment is not valid for this field element type".to_string(),
            ));
        }

        Ok(slice::from_raw_parts(p as *const Self, len))
    }

    fn zeroed_vector(n: usize) -> Vec<Self> {
        // this uses a specialized vector initialization code which requests zero-filled memory
        // from the OS; unfortunately, this works only for built-in types and we can't use
        // Self::ZERO here as much less efficient initialization procedure will be invoked.
        // We also use u64 to make sure the memory is aligned correctly for our element size.
        let result = vec![0u64; n];

        // translate a zero-filled vector of u64s into a vector of base field elements
        let mut v = core::mem::ManuallyDrop::new(result);
        let p = v.as_mut_ptr();
        let len = v.len();
        let cap = v.capacity();
        unsafe { Vec::from_raw_parts(p as *mut Self, len, cap) }
    }

    fn as_base_elements(elements: &[Self]) -> &[Self::BaseField] {
        elements
    }
}

impl<A: NativeMontMul> StarkField for AccelBaseElementRisc0<A> {
    /// sage: MODULUS = 2^64 - 2^32 + 1 \
    /// sage: GF(MODULUS).is_prime_field() \
    /// True \
    /// sage: GF(MODULUS).order() \
    /// 18446744069414584321
    const MODULUS: Self::PositiveInteger = M;
    const MODULUS_BITS: u32 = 64;

    /// sage: GF(MODULUS).primitive_element() \
    /// 7
    const GENERATOR: Self = Self::convert_into(7);

    /// sage: is_odd((MODULUS - 1) / 2^32) \
    /// True
    const TWO_ADICITY: u32 = 32;

    /// sage: k = (MODULUS - 1) / 2^32 \
    /// sage: GF(MODULUS).primitive_element()^k \
    /// 1753635133440165772
    const TWO_ADIC_ROOT_OF_UNITY: Self = Self::convert_into(G);

    fn get_modulus_le_bytes() -> Vec<u8> {
        M.to_le_bytes().to_vec()
    }

    #[inline]
    fn as_int(&self) -> Self::PositiveInteger {
        mont_red_cst(self.val as u128)
    }
}

impl<A: NativeMontMul> Randomizable for AccelBaseElementRisc0<A> {
    const VALUE_SIZE: usize = Self::ELEMENT_BYTES;

    fn from_random_bytes(bytes: &[u8]) -> Option<Self> {
        Self::try_from(bytes).ok()
    }
}

impl<A: NativeMontMul> Display for AccelBaseElementRisc0<A> {
    fn fmt(&self, f: &mut Formatter) -> core::fmt::Result {
        write!(f, "{}", self.as_int())
    }
}

// EQUALITY CHECKS
// ================================================================================================

impl<A: NativeMontMul> PartialEq for AccelBaseElementRisc0<A> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        equals(self.val, other.val) == 0xFFFFFFFFFFFFFFFF
    }
}

impl<A: NativeMontMul> Eq for AccelBaseElementRisc0<A> {}

// OVERLOADED OPERATORS
// ================================================================================================

impl<A: NativeMontMul> Add for AccelBaseElementRisc0<A> {
    type Output = Self;

    #[inline]
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn add(self, rhs: Self) -> Self {
        // We compute a + b = a - (p - b).
        let (x1, c1) = self.val.overflowing_sub(M - rhs.val);
        let adj = 0u32.wrapping_sub(c1 as u32);
        Self::from_mont(x1.wrapping_sub(adj as u64))
    }
}

impl<A: NativeMontMul> AddAssign for AccelBaseElementRisc0<A> {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs
    }
}

impl<A: NativeMontMul> Sub for AccelBaseElementRisc0<A> {
    type Output = Self;

    #[inline]
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn sub(self, rhs: Self) -> Self {
        let (x1, c1) = self.val.overflowing_sub(rhs.val);
        let adj = 0u32.wrapping_sub(c1 as u32);
        Self::from_mont(x1.wrapping_sub(adj as u64))
    }
}

impl<A: NativeMontMul> SubAssign for AccelBaseElementRisc0<A> {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        *self = *self - rhs;
    }
}

impl<A: NativeMontMul> Mul for AccelBaseElementRisc0<A> {
    type Output = Self;

    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self::from_mont(mont_red_cst((self.val as u128) * (rhs.val as u128)))
    }
}

impl<A: NativeMontMul> MulAssign for AccelBaseElementRisc0<A> {
    #[inline]
    fn mul_assign(&mut self, rhs: Self) {
        *self = *self * rhs
    }
}

impl<A: NativeMontMul> Div for AccelBaseElementRisc0<A> {
    type Output = Self;

    #[inline]
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn div(self, rhs: Self) -> Self {
        self * rhs.inv()
    }
}

impl<A: NativeMontMul> DivAssign for AccelBaseElementRisc0<A> {
    #[inline]
    fn div_assign(&mut self, rhs: Self) {
        *self = *self / rhs
    }
}

impl<A: NativeMontMul> Neg for AccelBaseElementRisc0<A> {
    type Output = Self;

    #[inline]
    fn neg(self) -> Self {
        Self::ZERO - self
    }
}

// QUADRATIC EXTENSION
// ================================================================================================

/// Defines a quadratic extension of the base field over an irreducible polynomial x<sup>2</sup> -
/// x + 2. Thus, an extension element is defined as α + β * φ, where φ is a root of this polynomial,
/// and α and β are base field elements.
impl<A: NativeMontMul> ExtensibleField<2> for AccelBaseElementRisc0<A> {
    #[inline]
    fn mul(a: [Self; 2], b: [Self; 2]) -> [Self; 2] {
        let r = A::native_mul_ext([a[0].val, a[1].val], [b[0].val, b[1].val]);
        [Self::from_mont(r[0]), Self::from_mont(r[1])]
    }

    #[inline(always)]
    fn mul_base(a: [Self; 2], b: Self) -> [Self; 2] {
        // multiplying an extension field element by a base field element requires just 2
        // multiplications in the base field.
        [a[0] * b, a[1] * b]
    }

    #[inline(always)]
    fn frobenius(x: [Self; 2]) -> [Self; 2] {
        [x[0] + x[1], -x[1]]
    }

    fn use_hint(a: [Self; 2]) -> Option<[Self; 2]> {
        #[cfg(feature = "use-hints")]
        {
            // means we are running as part of the verifier
            let k = [a[0].val, a[1].val];
            if let Some(res) = INV_NONDET_QUAD.lock().get(&k) {
                let res_c = res.clone();
                return Some([Self::convert_into(res_c[0]), Self::convert_into(res_c[1])]);
            } else {
                return None;
            }
        }
        None
    }

    fn save_hint(a: [Self; 2], b: [Self; 2]) -> () {
        #[cfg(all(feature = "generate-hints", feature = "std"))]
        {
            // means we are running as part of the prover
            INV_NONDET_QUAD.lock().insert(
                [a[0].as_int(), a[1].as_int()],
                [b[0].as_int(), b[1].as_int()],
            );
            // println!("inserted into INV_NONDET_QUAD: {:?} => {:?}", a, b);
        }
    }
}

// CUBIC EXTENSION
// ================================================================================================

/// Defines a cubic extension of the base field over an irreducible polynomial x<sup>3</sup> -
/// x - 1. Thus, an extension element is defined as α + β * φ + γ * φ^2, where φ is a root of this
/// polynomial, and α, β and γ are base field elements.
impl<A: NativeMontMul> ExtensibleField<3> for AccelBaseElementRisc0<A> {
    #[inline(always)]
    fn mul(a: [Self; 3], b: [Self; 3]) -> [Self; 3] {
        // performs multiplication in the extension field using 6 multiplications, 9 additions,
        // and 4 subtractions in the base field. overall, a single multiplication in the extension
        // field is roughly equal to 12 multiplications in the base field.
        let a0b0 = a[0] * b[0];
        let a1b1 = a[1] * b[1];
        let a2b2 = a[2] * b[2];

        let a0b0_a0b1_a1b0_a1b1 = (a[0] + a[1]) * (b[0] + b[1]);
        let a0b0_a0b2_a2b0_a2b2 = (a[0] + a[2]) * (b[0] + b[2]);
        let a1b1_a1b2_a2b1_a2b2 = (a[1] + a[2]) * (b[1] + b[2]);

        let a0b0_minus_a1b1 = a0b0 - a1b1;

        let a0b0_a1b2_a2b1 = a1b1_a1b2_a2b1_a2b2 + a0b0_minus_a1b1 - a2b2;
        let a0b1_a1b0_a1b2_a2b1_a2b2 =
            a0b0_a0b1_a1b0_a1b1 + a1b1_a1b2_a2b1_a2b2 - a1b1.double() - a0b0;
        let a0b2_a1b1_a2b0_a2b2 = a0b0_a0b2_a2b0_a2b2 - a0b0_minus_a1b1;

        [
            a0b0_a1b2_a2b1,
            a0b1_a1b0_a1b2_a2b1_a2b2,
            a0b2_a1b1_a2b0_a2b2,
        ]
    }

    #[inline(always)]
    fn mul_base(a: [Self; 3], b: Self) -> [Self; 3] {
        // multiplying an extension field element by a base field element requires just 3
        // multiplications in the base field.
        [a[0] * b, a[1] * b, a[2] * b]
    }

    #[inline(always)]
    fn frobenius(x: [Self; 3]) -> [Self; 3] {
        // coefficients were computed using SageMath
        [
            x[0] + Self::convert_into(10615703402128488253) * x[1]
                + Self::convert_into(6700183068485440220) * x[2],
            Self::convert_into(10050274602728160328) * x[1]
                + Self::convert_into(14531223735771536287) * x[2],
            Self::convert_into(11746561000929144102) * x[1]
                + Self::convert_into(8396469466686423992) * x[2],
        ]
    }

    fn use_hint(a: [Self; 3]) -> Option<[Self; 3]> {
        todo!()
    }

    fn save_hint(a: [Self; 3], b: [Self; 3]) -> () {
        todo!()
    }
}

// TYPE CONVERSIONS
// ================================================================================================

impl<A: NativeMontMul> From<u128> for AccelBaseElementRisc0<A> {
    /// Converts a 128-bit value into a field element.
    fn from(x: u128) -> Self {
        //const R3: u128 = 1 (= 2^192 mod M );// thus we get that mont_red_var((mont_red_var(x) as u128) * R3) becomes
        //Self(mont_red_var(mont_red_var(x) as u128))  // Variable time implementation
        Self::from_mont(mont_red_cst(mont_red_cst(x) as u128)) // Constant time implementation
    }
}

impl<A: NativeMontMul> From<u64> for AccelBaseElementRisc0<A> {
    /// Converts a 64-bit value into a field element. If the value is greater than or equal to
    /// the field modulus, modular reduction is silently performed.
    fn from(value: u64) -> Self {
        Self::convert_into(value)
    }
}

impl<A: NativeMontMul> From<u32> for AccelBaseElementRisc0<A> {
    /// Converts a 32-bit value into a field element.
    fn from(value: u32) -> Self {
        Self::convert_into(value as u64)
    }
}

impl<A: NativeMontMul> From<u16> for AccelBaseElementRisc0<A> {
    /// Converts a 16-bit value into a field element.
    fn from(value: u16) -> Self {
        Self::convert_into(value as u64)
    }
}

impl<A: NativeMontMul> From<u8> for AccelBaseElementRisc0<A> {
    /// Converts an 8-bit value into a field element.
    fn from(value: u8) -> Self {
        Self::convert_into(value as u64)
    }
}

impl<A: NativeMontMul> From<[u8; 8]> for AccelBaseElementRisc0<A> {
    /// Converts the value encoded in an array of 8 bytes into a field element. The bytes are
    /// assumed to encode the element in the canonical representation in little-endian byte order.
    /// If the value is greater than or equal to the field modulus, modular reduction is silently
    /// performed.
    fn from(bytes: [u8; 8]) -> Self {
        let value = u64::from_le_bytes(bytes);
        Self::convert_into(value)
    }
}

impl<'a, A: NativeMontMul> TryFrom<&'a [u8]> for AccelBaseElementRisc0<A> {
    type Error = DeserializationError;

    /// Converts a slice of bytes into a field element; returns error if the value encoded in bytes
    /// is not a valid field element. The bytes are assumed to encode the element in the canonical
    /// representation in little-endian byte order.
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        if bytes.len() < ELEMENT_BYTES {
            return Err(DeserializationError::InvalidValue(format!(
                "not enough bytes for a full field element; expected {} bytes, but was {} bytes",
                ELEMENT_BYTES,
                bytes.len(),
            )));
        }
        if bytes.len() > ELEMENT_BYTES {
            return Err(DeserializationError::InvalidValue(format!(
                "too many bytes for a field element; expected {} bytes, but was {} bytes",
                ELEMENT_BYTES,
                bytes.len(),
            )));
        }
        let value = bytes
            .try_into()
            .map(u64::from_le_bytes)
            .map_err(|error| DeserializationError::UnknownError(format!("{}", error)))?;
        if value >= M {
            return Err(DeserializationError::InvalidValue(format!(
                "invalid field element: value {} is greater than or equal to the field modulus",
                value
            )));
        }
        Ok(Self::convert_into(value))
    }
}

impl<A: NativeMontMul> AsBytes for AccelBaseElementRisc0<A> {
    fn as_bytes(&self) -> &[u8] {
        // TODO: take endianness into account
        let self_ptr: *const AccelBaseElementRisc0<A> = self;
        unsafe { slice::from_raw_parts(self_ptr as *const u8, ELEMENT_BYTES) }
    }
}

// SERIALIZATION / DESERIALIZATION
// ------------------------------------------------------------------------------------------------

impl<A: NativeMontMul> Serializable for AccelBaseElementRisc0<A> {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        // convert from Montgomery representation into canonical representation
        target.write_u8_slice(&self.as_int().to_le_bytes());
    }
}

impl<A: NativeMontMul> Deserializable for AccelBaseElementRisc0<A> {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let value = source.read_u64()?;
        if value >= M {
            return Err(DeserializationError::InvalidValue(format!(
                "invalid field element: value {} is greater than or equal to the field modulus",
                value
            )));
        }
        Ok(Self::convert_into(value))
    }
}

/// Squares the base N number of times and multiplies the result by the tail value.
#[inline(always)]
fn exp_acc<const N: usize, A: NativeMontMul>(
    base: AccelBaseElementRisc0<A>,
    tail: AccelBaseElementRisc0<A>,
) -> AccelBaseElementRisc0<A> {
    let mut result = base;
    for _ in 0..N {
        result = result.square();
    }
    result * tail
}

/// Montgomery reduction (variable time)
#[allow(dead_code)]
#[inline(always)]
const fn mont_red_var(x: u128) -> u64 {
    const NPRIME: u64 = 4294967297;
    let q = (((x as u64) as u128) * (NPRIME as u128)) as u64;
    let m = (q as u128) * (M as u128);
    let y = (((x as i128).wrapping_sub(m as i128)) >> 64) as i64;
    if x < m {
        (y + (M as i64)) as u64
    } else {
        y as u64
    }
}

/// Montgomery reduction (constant time)
#[inline(always)]
const fn mont_red_cst(x: u128) -> u64 {
    // See reference above for a description of the following implementation.
    let xl = x as u64;
    let xh = (x >> 64) as u64;
    let (a, e) = xl.overflowing_add(xl << 32);

    let b = a.wrapping_sub(a >> 32).wrapping_sub(e as u64);

    let (r, c) = xh.overflowing_sub(b);
    r.wrapping_sub(0u32.wrapping_sub(c as u32) as u64)
}

/// Test of equality between two BaseField elements; return value is
/// 0xFFFFFFFFFFFFFFFF if the two values are equal, or 0 otherwise.
#[inline(always)]
pub fn equals(lhs: u64, rhs: u64) -> u64 {
    let t = lhs ^ rhs;
    !((((t | t.wrapping_neg()) as i64) >> 63) as u64)
}
