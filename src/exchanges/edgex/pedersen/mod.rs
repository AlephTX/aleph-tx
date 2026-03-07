use num_bigint::BigUint;
use num_traits::{Zero, One, Num};

mod pedersen_points;
use pedersen_points::CONSTANT_POINTS;

// StarkEx curve parameters
const FIELD_PRIME_HEX: &str = "800000000000011000000000000000000000000000000000000000000000001";
const ALPHA: u64 = 1;

pub struct PedersenHash {
    field_prime: BigUint,
    shift_point: (BigUint, BigUint),
    constant_points: Vec<(BigUint, BigUint)>,
}

impl PedersenHash {
    pub fn new() -> Self {
        let field_prime = BigUint::from_str_radix(FIELD_PRIME_HEX, 16).unwrap();

        // Load all constant points
        let constant_points: Vec<(BigUint, BigUint)> = CONSTANT_POINTS
            .iter()
            .map(|(x_hex, y_hex)| {
                let x = BigUint::from_str_radix(x_hex, 16).unwrap();
                let y = BigUint::from_str_radix(y_hex, 16).unwrap();
                (x, y)
            })
            .collect();

        // Shift point is the first constant point
        let shift_point = constant_points[0].clone();

        Self {
            field_prime,
            shift_point,
            constant_points,
        }
    }

    fn div_mod(&self, n: &BigUint, m: &BigUint) -> BigUint {
        // Calculate (n / m) mod p using modular inverse
        let m_inv = m.modpow(&(&self.field_prime - BigUint::from(2u32)), &self.field_prime);
        (n * m_inv) % &self.field_prime
    }

    fn ec_add(&self, p1: &(BigUint, BigUint), p2: &(BigUint, BigUint)) -> (BigUint, BigUint) {
        if p1.0 == p2.0 {
            if (&p1.1 + &p2.1) % &self.field_prime == BigUint::zero() {
                panic!("Points are negatives of each other");
            }
            return self.ec_double(p1);
        }

        // Calculate slope
        let dy = (&p2.1 + &self.field_prime - &p1.1) % &self.field_prime;
        let dx = (&p2.0 + &self.field_prime - &p1.0) % &self.field_prime;
        let slope = self.div_mod(&dy, &dx);

        // Calculate new point
        let x3 = (&slope * &slope + &self.field_prime + &self.field_prime - &p1.0 - &p2.0) % &self.field_prime;
        let y3 = (&slope * ((&p1.0 + &self.field_prime - &x3) % &self.field_prime) + &self.field_prime - &p1.1) % &self.field_prime;

        (x3, y3)
    }

    fn ec_double(&self, p: &(BigUint, BigUint)) -> (BigUint, BigUint) {
        // Calculate slope: (3 * x^2 + ALPHA) / (2 * y)
        let numerator = (BigUint::from(3u32) * &p.0 * &p.0 + BigUint::from(ALPHA)) % &self.field_prime;
        let denominator = (BigUint::from(2u32) * &p.1) % &self.field_prime;
        let slope = self.div_mod(&numerator, &denominator);

        // Calculate new point
        let x3 = (&slope * &slope + &self.field_prime + &self.field_prime - BigUint::from(2u32) * &p.0) % &self.field_prime;
        let y3 = (&slope * ((&p.0 + &self.field_prime - &x3) % &self.field_prime) + &self.field_prime - &p.1) % &self.field_prime;

        (x3, y3)
    }

    pub fn hash(&self, elements: &[BigUint]) -> BigUint {
        let mut point = self.shift_point.clone();

        for (i, element) in elements.iter().enumerate() {
            let start_idx = 2 + i * 252;

            let mut elem = element.clone();
            for j in 0..252 {
                if start_idx + j >= self.constant_points.len() {
                    panic!("Insufficient constant points");
                }

                let pt = &self.constant_points[start_idx + j];

                if point.0 == pt.0 {
                    panic!("Unhashable input: point collision detected");
                }

                if &elem & BigUint::one() == BigUint::one() {
                    point = self.ec_add(&point, pt);
                }
                elem >>= 1;
            }

            if elem != BigUint::zero() {
                panic!("Element too large");
            }
        }

        point.0
    }
}
