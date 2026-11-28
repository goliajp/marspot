//! `exp` and `ln` that give the same bits on every machine.
//!
//! `f64::exp` and `f64::ln` call the platform's libm, and Apple's and
//! glibc's differ in the last bit for some arguments.  A decision that
//! compares such a value with a threshold (a rest drawn against its
//! chance, a speed against a band) then goes the other way on one machine
//! and the plans part from there.  These are the fdlibm algorithms, as
//! musl has them: only IEEE additions, multiplications and divisions,
//! which every machine rounds alike (Rust never fuses them), so the
//! result is the same everywhere and within one unit in the last place.
//! The constants are fdlibm's, by their bits.

const LN2_HI: f64 = f64::from_bits(0x3fe6_2e42_fee0_0000);
const LN2_LO: f64 = f64::from_bits(0x3dea_39ef_3579_3c76);
const P1: f64 = f64::from_bits(0x3fc5_5555_5555_553e);
const P2: f64 = f64::from_bits(0xbf66_c16c_16be_bd93);
const P3: f64 = f64::from_bits(0x3f11_566a_af25_de2c);
const P4: f64 = f64::from_bits(0xbebb_bd41_c5d2_6bf1);
const P5: f64 = f64::from_bits(0x3e66_3769_72be_a4d0);

/// `y` times two to the `k`, exactly while the result is normal.
fn scale(y: f64, k: i32) -> f64 {
    let pow2 = |k: i32| f64::from_bits(((0x3ff + k) as u64) << 52);
    if k > 1023 {
        y * pow2(1023) * pow2(k - 1023)
    } else if k < -1022 {
        y * pow2(-1022) * pow2((k + 1022).max(-1022))
    } else {
        y * pow2(k)
    }
}

/// e to the `x`.
pub fn exp(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x > 709.782_712_893_384 {
        return f64::INFINITY;
    }
    if x < -745.133_219_101_941_1 {
        return 0.0;
    }
    let hx = (x.to_bits() >> 32) as u32 & 0x7fff_ffff;
    let (k, hi, lo, r) = if hx > 0x3fd6_2e42 {
        // |x| > ln2 / 2
        let k = if hx >= 0x3ff0_a2b2 { (std::f64::consts::LOG2_E * x + if x < 0.0 { -0.5 } else { 0.5 }) as i32 } else if x < 0.0 { -1 } else { 1 };
        let hi = x - f64::from(k) * LN2_HI;
        let lo = f64::from(k) * LN2_LO;
        (k, hi, lo, hi - lo)
    } else if hx > 0x3e30_0000 {
        (0, x, 0.0, x)
    } else {
        return 1.0 + x;
    };
    let xx = r * r;
    let c = r - xx * (P1 + xx * (P2 + xx * (P3 + xx * (P4 + xx * P5))));
    let y = 1.0 + (r * c / (2.0 - c) - lo + hi);
    if k == 0 { y } else { scale(y, k) }
}

const LG1: f64 = f64::from_bits(0x3fe5_5555_5555_5593);
const LG2: f64 = f64::from_bits(0x3fd9_9999_9997_fa04);
const LG3: f64 = f64::from_bits(0x3fd2_4924_9422_9359);
const LG4: f64 = f64::from_bits(0x3fcc_71c5_1d8e_78af);
const LG5: f64 = f64::from_bits(0x3fc7_4664_96cb_03de);
const LG6: f64 = f64::from_bits(0x3fc3_9a09_d078_c69f);
const LG7: f64 = f64::from_bits(0x3fc2_f112_df3e_5244);

/// The natural logarithm of `x`.
pub fn ln(x: f64) -> f64 {
    let mut bits = x.to_bits();
    let mut hx = (bits >> 32) as u32;
    let mut k: i32 = 0;
    if hx < 0x0010_0000 || hx >> 31 != 0 {
        if bits << 1 == 0 {
            return f64::NEG_INFINITY;
        }
        if hx >> 31 != 0 {
            return f64::NAN;
        }
        // subnormal: scale up
        k -= 54;
        bits = (x * f64::from_bits(0x4350_0000_0000_0000)).to_bits();
        hx = (bits >> 32) as u32;
    } else if hx >= 0x7ff0_0000 {
        return x;
    } else if hx == 0x3ff0_0000 && bits << 32 == 0 {
        return 0.0;
    }
    // reduce into [sqrt(2)/2, sqrt(2)]
    hx += 0x3ff0_0000 - 0x3fe6_a09e;
    k += (hx >> 20) as i32 - 0x3ff;
    hx = (hx & 0x000f_ffff) + 0x3fe6_a09e;
    let m = f64::from_bits((u64::from(hx) << 32) | (bits & 0xffff_ffff));
    let f = m - 1.0;
    let hfsq = 0.5 * f * f;
    let s = f / (2.0 + f);
    let z = s * s;
    let w = z * z;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    let r = t2 + t1;
    let dk = f64::from(k);
    s * (hfsq + r) + dk * LN2_LO - hfsq + f + dk * LN2_HI
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ulps(a: f64, b: f64) -> u64 {
        (a.to_bits() as i64 - b.to_bits() as i64).unsigned_abs()
    }

    #[test]
    fn detmath_agrees_with_the_platform_within_one_ulp() {
        let mut worst = (0, 0);
        let mut x = -740.0;
        while x < 709.0 {
            worst.0 = worst.0.max(ulps(exp(x), x.exp()));
            x += 0.013_7;
        }
        let mut y = 1e-300;
        while y < 1e300 {
            worst.1 = worst.1.max(ulps(ln(y), y.ln()));
            y *= 1.013_7;
        }
        assert!(worst.0 <= 1 && worst.1 <= 1, "{worst:?}");
        assert_eq!((exp(0.0), ln(1.0), exp(f64::NEG_INFINITY), ln(0.0)), (1.0, 0.0, 0.0, f64::NEG_INFINITY));
        assert!(ln(-1.0).is_nan() && exp(f64::NAN).is_nan() && exp(1000.0).is_infinite());
        assert!(ulps(ln(f64::MIN_POSITIVE / 8.0), (f64::MIN_POSITIVE / 8.0).ln()) <= 1);
    }

    #[test]
    fn detmath_known_bits() {
        // outputs recorded once: a machine that reads other bits rounds
        // differently, and nothing here may depend on it (exp(1) is one unit
        // above the nearest double to e)
        assert_eq!(exp(1.0).to_bits(), 4_613_303_445_314_885_482);
        assert_eq!(exp(1.0).to_bits(), std::f64::consts::E.to_bits() + 1);
        assert_eq!(ln(2.0).to_bits(), std::f64::consts::LN_2.to_bits());
    }
}
