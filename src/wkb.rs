//! Just enough Well-Known Binary to test "is this point inside that area?".

use anyhow::{bail, ensure};

#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point(f64, f64),
    /// Lines and points we only use for distances; kept as their vertices.
    Path(Vec<(f64, f64)>),
    /// Polygons: each a list of rings (outer first), each ring a list of (x, y).
    Polygons(Vec<Vec<Vec<(f64, f64)>>>),
    Collection(Vec<Geometry>),
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
    little: bool,
}

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        ensure!(self.pos + N <= self.b.len(), "truncated WKB");
        let out: [u8; N] = self.b[self.pos..self.pos + N].try_into().unwrap();
        self.pos += N;
        Ok(out)
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        let b = self.take::<4>()?;
        Ok(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn f64(&mut self) -> anyhow::Result<f64> {
        let b = self.take::<8>()?;
        Ok(if self.little { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
    }
}

impl Geometry {
    pub fn parse(wkb: &[u8]) -> anyhow::Result<Self> {
        let mut c = Cursor { b: wkb, pos: 0, little: true };
        Self::read(&mut c)
    }

    fn read(c: &mut Cursor) -> anyhow::Result<Self> {
        c.little = c.take::<1>()?[0] == 1;
        let raw = c.u32()?;
        // ISO (1000s = Z, 2000s = M, 3000s = ZM) and EWKB (high flag bits) dimension variants.
        let dims_iso = (raw % 10_000) / 1000;
        let ewkb_z = raw & 0x8000_0000 != 0;
        let ewkb_m = raw & 0x4000_0000 != 0;
        if raw & 0x2000_0000 != 0 {
            c.u32()?; // EWKB SRID
        }
        let kind = (raw & 0x0FFF_FFFF) % 1000;
        let extra = match dims_iso {
            1 | 2 => 1,
            3 => 2,
            _ => usize::from(ewkb_z) + usize::from(ewkb_m),
        };
        let point = |c: &mut Cursor| -> anyhow::Result<(f64, f64)> {
            let (x, y) = (c.f64()?, c.f64()?);
            for _ in 0..extra {
                c.f64()?;
            }
            Ok((x, y))
        };
        let points = |c: &mut Cursor| -> anyhow::Result<Vec<(f64, f64)>> {
            let n = c.u32()? as usize;
            ensure!(n <= c.b.len() / 16, "implausible point count");
            (0..n).map(|_| point(c)).collect()
        };
        let polygon = |c: &mut Cursor| -> anyhow::Result<Vec<Vec<(f64, f64)>>> {
            let rings = c.u32()? as usize;
            ensure!(rings <= c.b.len() / 4, "implausible ring count");
            (0..rings).map(|_| points(c)).collect()
        };
        Ok(match kind {
            1 => {
                let (x, y) = point(c)?;
                Geometry::Point(x, y)
            }
            2 => Geometry::Path(points(c)?),
            3 => Geometry::Polygons(vec![polygon(c)?]),
            4..=7 => {
                let n = c.u32()? as usize;
                ensure!(n <= c.b.len() / 5, "implausible part count");
                let parts = (0..n).map(|_| Self::read(c)).collect::<anyhow::Result<Vec<_>>>()?;
                if kind == 6 {
                    Geometry::Polygons(
                        parts
                            .into_iter()
                            .flat_map(|p| if let Geometry::Polygons(v) = p { v } else { vec![] })
                            .collect(),
                    )
                } else {
                    Geometry::Collection(parts)
                }
            }
            other => bail!("unsupported WKB geometry type {other}"),
        })
    }

    /// Is (x, y) inside this geometry's area? (Even-odd rule, so holes work.)
    pub fn contains(&self, x: f64, y: f64) -> bool {
        match self {
            Geometry::Polygons(polys) => polys
                .iter()
                .any(|rings| rings.iter().filter(|ring| ring_crossings(ring, x, y)).count() % 2 == 1),
            Geometry::Collection(parts) => parts.iter().any(|g| g.contains(x, y)),
            _ => false,
        }
    }

    pub fn as_point(&self) -> Option<(f64, f64)> {
        match self {
            Geometry::Point(x, y) => Some((*x, *y)),
            _ => None,
        }
    }
}

/// Ray casting: does a ray from (x, y) towards +x cross this ring an odd number of times?
fn ring_crossings(ring: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[(i + n - 1) % n];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkb_polygon(rings: &[&[(f64, f64)]]) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend(3u32.to_le_bytes());
        b.extend((rings.len() as u32).to_le_bytes());
        for ring in rings {
            b.extend((ring.len() as u32).to_le_bytes());
            for (x, y) in *ring {
                b.extend(x.to_le_bytes());
                b.extend(y.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn polygon_with_hole() {
        let outer: &[(f64, f64)] = &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)];
        let hole: &[(f64, f64)] = &[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)];
        let g = Geometry::parse(&wkb_polygon(&[outer, hole])).unwrap();
        assert!(g.contains(1.0, 1.0));
        assert!(!g.contains(5.0, 5.0), "inside the hole");
        assert!(!g.contains(11.0, 5.0));
    }

    #[test]
    fn multipolygon_and_point_big_endian() {
        let square: &[(f64, f64)] = &[(20.0, 20.0), (21.0, 20.0), (21.0, 21.0), (20.0, 21.0), (20.0, 20.0)];
        let mut mp = vec![1u8];
        mp.extend(6u32.to_le_bytes());
        mp.extend(1u32.to_le_bytes());
        mp.extend(wkb_polygon(&[square]));
        let g = Geometry::parse(&mp).unwrap();
        assert!(g.contains(20.5, 20.5));
        assert!(!g.contains(0.0, 0.0));

        let mut pt = vec![0u8];
        pt.extend(1u32.to_be_bytes());
        pt.extend(2.5f64.to_be_bytes());
        pt.extend((-3.5f64).to_be_bytes());
        assert_eq!(Geometry::parse(&pt).unwrap().as_point(), Some((2.5, -3.5)));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(Geometry::parse(&[1, 3, 0, 0]).is_err());
        assert!(Geometry::parse(&[1, 99, 0, 0, 0]).is_err());
    }
}
