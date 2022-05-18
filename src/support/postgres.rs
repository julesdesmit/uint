//! Support for the [`postgres`](https://crates.io/crates/postgres) crate.
#![cfg(feature = "postgres")]

use crate::{utils::trim_end_vec, Uint};
use bytes::{BufMut, BytesMut};
use postgres_types::{to_sql_checked, IsNull, ToSql, Type, WrongType};
use std::error::Error;
use thiserror::Error;

type BoxedError = Box<dyn Error + Sync + Send + 'static>;

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum ToSqlError {
    #[error("Uint<{0}> value too large to fit target type {1}")]
    Overflow(usize, Type),
}

/// Convert to Postgres types.
///
/// Compatible [Postgres data types][dt] are:
///
/// * `BOOL`, `SMALLINT`, `INTEGER`, `BIGINT` which are 1, 16, 32 and 64 bit
///   signed integers respectively.
/// * `OID` which is a 32 bit unsigned integer.
/// * `FLOAT`, `DOUBLE PRECISION` which are 32 and 64 bit floating point.
/// * `DECIMAL` and `NUMERIC`, which are variable length.
/// * `MONEY` which is a 64 bit integer with two decimals.
/// * `BYTEA`, `BIT`, `VARBIT` interpreted as a big-endian binary number.
/// * `CHAR`, `VARCHAR`, `TEXT` as `0x`-prefixed big-endian hex strings.
/// * TODO: `JSON`, `JSONB` as a Serde compatible JSON value (requires `serde`
///   feature).
///
/// Note: [`Uint`]s are never null, use [`Option<Uint>`] instead.
///
/// # Errors
///
/// Returns an error when trying to convert to a value that is too small to fit
/// the number. Note that this depends on the value, not the type, so a
/// [`Uint<256>`] can be stored in a `SMALLINT` column, as long as the values
/// are less than $2^{16}$.
///
/// # Implementation details
///
/// The Postgres binary formats are used in the wire-protocol and the
/// the `COPY BINARY` command, but they have very little documentation. You are
/// pointed to the source code, for example this is the implementation of the
/// the `NUMERIC` type serializer: [`numeric.c`][numeric].
///
/// [dt]:https://www.postgresql.org/docs/9.5/datatype.html
/// [numeric]: https://github.com/postgres/postgres/blob/05a5a1775c89f6beb326725282e7eea1373cbec8/src/backend/utils/adt/numeric.c#L1082
impl<const BITS: usize, const LIMBS: usize> ToSql for Uint<BITS, LIMBS> {
    fn accepts(ty: &Type) -> bool {
        matches!(*ty, |Type::BOOL| Type::CHAR
            | Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::FLOAT4
            | Type::FLOAT8
            | Type::MONEY
            | Type::NUMERIC
            | Type::BYTEA
            | Type::TEXT
            | Type::VARCHAR
            | Type::JSON
            | Type::JSONB
            | Type::BIT
            | Type::VARBIT)
    }

    // See <https://github.com/sfackler/rust-postgres/blob/38da7fa8fe0067f47b60c147ccdaa214ab5f5211/postgres-protocol/src/types/mod.rs>
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxedError> {
        match *ty {
            // Big-endian simple types
            // Note `BufMut::put_*` methods write big-endian by default.
            Type::BOOL => out.put_u8(if bool::try_from(*self)? { 1 } else { 0 }),
            Type::INT2 => out.put_i16(self.try_into()?),
            Type::INT4 => out.put_i32(self.try_into()?),
            Type::OID => out.put_u32(self.try_into()?),
            Type::INT8 => out.put_i64(self.try_into()?),
            Type::FLOAT4 => out.put_f32(self.try_into()?),
            Type::FLOAT8 => out.put_f64(self.try_into()?),
            Type::MONEY => {
                // Like i64, but with two decimals.
                out.put_i64(
                    i64::try_from(self)?
                        .checked_mul(100)
                        .ok_or_else(|| ToSqlError::Overflow(BITS, ty.clone()))?,
                );
            }

            // Binary strings
            Type::BYTEA => out.put_slice(&self.to_be_bytes_vec()),
            Type::BIT | Type::VARBIT => {
                // Bit in little-endian so the the first bit is the least significant.
                // Length must be at least one bit.
                if BITS == 0 {
                    if *ty == Type::BIT {
                        // `bit(0)` is not a valid type, but varbit can be empty.
                        return Err(Box::new(WrongType::new::<Self>(ty.clone())));
                    }
                    out.put_i32(0);
                } else {
                    // Bits are output in little-endian order, but padded at the
                    // least significant end.
                    let padding = if (BITS % 8) == 0 { 0 } else { 8 - BITS % 8 };
                    out.put_i32(Self::BITS.try_into()?);
                    let bytes = self.as_le_bytes();
                    let mut bytes = bytes.iter().rev();
                    let mut shifted = bytes.next().unwrap() << padding;
                    for byte in bytes {
                        shifted |= if padding > 0 {
                            byte >> (8 - padding)
                        } else {
                            0
                        };
                        out.put_u8(shifted);
                        shifted = byte << padding;
                    }
                    out.put_u8(shifted);
                }
            }

            // Hex strings
            Type::CHAR | Type::TEXT | Type::VARCHAR => {
                out.put_slice(format!("{:#x}", self).as_bytes());
            }

            // Binary coded decimal types
            // See <https://github.com/postgres/postgres/blob/05a5a1775c89f6beb326725282e7eea1373cbec8/src/backend/utils/adt/numeric.c#L253>
            Type::NUMERIC => {
                // Everything is done in big-endian base 1000 digits.
                const BASE: u64 = 10000;
                let mut digits: Vec<_> = self.to_base_be(BASE).collect();
                let exponent = digits.len().saturating_sub(1).try_into()?;

                // Trailing zeros are removed.
                trim_end_vec(&mut digits, &0);

                out.put_i16(digits.len().try_into()?); // Number of digits.
                out.put_i16(exponent); // Exponent of first digit.
                out.put_i16(0); // sign: 0x0000 = positive, 0x4000 = negative.
                out.put_i16(0); // dscale: Number of digits to the right of the decimal point.
                for digit in digits {
                    debug_assert!(digit < BASE);
                    #[allow(clippy::cast_possible_truncation)] // 10000 < i16::MAX
                    out.put_i16(digit as i16);
                }
            }

            // Unsupported types
            _ => {
                return Err(Box::new(WrongType::new::<Self>(ty.clone())));
            }
        };
        Ok(IsNull::No)
    }

    to_sql_checked!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{const_for, nbytes, nlimbs};
    use approx::assert_ulps_eq;
    use hex_literal::hex;
    use postgres::{Client, NoTls};
    use proptest::{proptest, test_runner::Config as ProptestConfig};
    use std::{io::Read, sync::Mutex};

    #[test]
    fn test_basic() {
        #[allow(clippy::unreadable_literal)]
        const N: Uint<256, 4> = Uint::from_limbs([
            0xa8ec92344438aaf4_u64,
            0x9819ebdbd1faaab1_u64,
            0x573b1a7064c19c1a_u64,
            0xc85ef7d79691fe79_u64,
        ]);
        #[allow(clippy::needless_pass_by_value)]
        fn bytes(ty: Type) -> Vec<u8> {
            let mut out = BytesMut::new();
            N.to_sql(&ty, &mut out).unwrap();
            out.to_vec()
        }
        assert_eq!(bytes(Type::FLOAT4), hex!("7f800000")); // +inf
        assert_eq!(bytes(Type::FLOAT8), hex!("4fe90bdefaf2d240"));
        assert_eq!(bytes(Type::NUMERIC), hex!("0014001300000000000902760e3620f115a21c3b029709bc11e60b3e10d10d6900d123400def1c45091a147900f012f4"));
        assert_eq!(
            bytes(Type::BYTEA),
            hex!("c85ef7d79691fe79573b1a7064c19c1a9819ebdbd1faaab1a8ec92344438aaf4")
        );
        assert_eq!(
            bytes(Type::BIT),
            hex!("00000100c85ef7d79691fe79573b1a7064c19c1a9819ebdbd1faaab1a8ec92344438aaf4")
        );
        assert_eq!(
            bytes(Type::VARBIT),
            hex!("00000100c85ef7d79691fe79573b1a7064c19c1a9819ebdbd1faaab1a8ec92344438aaf4")
        );
        assert_eq!(bytes(Type::CHAR), hex!("307863383565663764373936393166653739353733623161373036346331396331613938313965626462643166616161623161386563393233343434333861616634"));
        assert_eq!(bytes(Type::TEXT), hex!("307863383565663764373936393166653739353733623161373036346331396331613938313965626462643166616161623161386563393233343434333861616634"));
        assert_eq!(bytes(Type::VARCHAR), hex!("307863383565663764373936393166653739353733623161373036346331396331613938313965626462643166616161623161386563393233343434333861616634"));
    }

    // Query the binary encoding of an SQL expression
    fn get_binary(client: &mut Client, expr: &str) -> Vec<u8> {
        let query = format!("COPY (SELECT {}) TO STDOUT WITH BINARY;", expr);

        // See <https://www.postgresql.org/docs/current/sql-copy.html>
        let mut reader = client.copy_out(&query).unwrap();
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();

        // Parse header
        let buf = {
            const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0";
            assert_eq!(&buf[..11], HEADER);
            &buf[11 + 4..]
        };

        // Skip extension headers (must be zero length)
        assert_eq!(&buf[..4], &0_u32.to_be_bytes());
        let buf = &buf[4..];

        // Tuple field count must be one
        assert_eq!(&buf[..2], &1_i16.to_be_bytes());
        let buf = &buf[2..];

        // Field length
        let len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
        let buf = &buf[4..];

        // Field data
        let data = &buf[..len];
        let buf = &buf[len..];

        // Trailer must be -1_i16
        assert_eq!(&buf, &(-1_i16).to_be_bytes());

        data.to_owned()
    }

    fn test_to<const BITS: usize, const LIMBS: usize>(
        client: &Mutex<Client>,
        value: Uint<BITS, LIMBS>,
        ty: &Type,
    ) {
        println!("testing {:?} {}", value, ty);

        // Encode value locally
        let mut serialized = BytesMut::new();
        let result = value.to_sql(ty, &mut serialized);
        if result.is_err() {
            // Skip values that are out of range for the type
            return;
        }
        // Skip floating point infinities
        if ty == &Type::FLOAT4 && f32::from(value).is_infinite() {
            return;
        }
        if ty == &Type::FLOAT8 && f64::from(value).is_infinite() {
            return;
        }
        // dbg!(hex::encode(&serialized));

        // Fetch ground truth value from Postgres
        let expr = match ty {
            &Type::BIT => format!(
                "B'{value:b}'::bit({bits})",
                value = value,
                bits = if BITS == 0 { 1 } else { BITS },
            ),
            &Type::VARBIT => format!("B'{value:b}'::varbit", value = value,),
            &Type::BYTEA => format!("'\\x{:x}'::bytea", value),
            &Type::CHAR => format!("'{:#x}'::char({})", value, 2 + 2 * nbytes(BITS)),
            &Type::TEXT | &Type::VARCHAR => format!("'{:#x}'::{}", value, ty.name()),
            _ => format!("{}::{}", value, ty.name()),
        };
        // dbg!(&expr);
        let ground_truth = {
            let mut client = client.lock().unwrap();
            get_binary(&mut client, &expr)
        };
        // dbg!(hex::encode(&ground_truth));

        // Compare with ground truth, for float we allow tiny rounding error
        if ty == &Type::FLOAT4 {
            let serialized = f32::from_be_bytes(serialized.as_ref().try_into().unwrap());
            let ground_truth = f32::from_be_bytes(ground_truth.try_into().unwrap());
            assert_ulps_eq!(serialized, ground_truth, max_ulps = 4);
        } else if ty == &Type::FLOAT8 {
            let serialized = f64::from_be_bytes(serialized.as_ref().try_into().unwrap());
            let ground_truth = f64::from_be_bytes(ground_truth.try_into().unwrap());
            assert_ulps_eq!(serialized, ground_truth, max_ulps = 4);
        } else {
            // Check that the value is exactly the same as the ground truth
            assert_eq!(serialized, ground_truth);
        }
    }

    // This test requires a live postgresql server.
    // To start a server, run:
    //
    //     docker run -it --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres
    //
    // Then run the test using:
    //
    //    PROPTEST_CASES=1000 cargo test --all-features -- --include-ignored
    // --nocapture postgres
    //
    #[test]
    #[ignore]
    fn test_postgres() {
        // docker run -it --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres
        let client = Client::connect("postgresql://postgres:postgres@localhost", NoTls).unwrap();
        let client = Mutex::new(client);

        const_for!(BITS in SIZES {
            const LIMBS: usize = nlimbs(BITS);

            // By default generates 256 random values per bit size. Configurable
            // with the `PROPTEST_CASES` env variable.
            let mut config = ProptestConfig::default();
            // No point in running many values for small sizes
            if BITS < 4 { config.cases = 16; };

            proptest!(config, |(value: Uint<BITS, LIMBS>)| {

                // Test based on which types value will fit
                let bits = value.bit_len();
                if bits <= 1 {
                    test_to(&client, value, &Type::BOOL);
                }
                if bits <= 15 {
                    test_to(&client, value, &Type::INT2);
                }
                if bits <= 31 {
                    test_to(&client, value, &Type::INT4);
                }
                if bits <= 32 {
                    test_to(&client, value, &Type::OID);
                }
                if bits <= 50 {
                    test_to(&client, value, &Type::MONEY);
                }
                if bits <= 63 {
                    test_to(&client, value, &Type::INT8);
                }

                // Floating points always work, except when the exponent
                // overflows. We map that to +∞, mut SQL rejects it. This
                // is handled in the `test_to` function.
                test_to(&client, value, &Type::FLOAT4);
                test_to(&client, value, &Type::FLOAT8);

                // Types that work for any size
                for ty in &[Type::NUMERIC, Type::BIT, Type::VARBIT, Type::BYTEA, Type::CHAR, Type::TEXT, Type::VARCHAR] {
                    test_to(&client, value, ty);
                }

            });
        });
    }
}