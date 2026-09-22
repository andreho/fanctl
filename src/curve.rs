/// A piecewise-linear control curve mapping a temperature (°C) to a fan duty
/// percentage (0..=100).
///
/// A curve is a list of `(temp, duty)` points. Points are sorted by temperature
/// ascending; the curve is clamped to the first/last duty below/above the
/// endpoints, and linearly interpolated between them.
use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

/// A single point on the control curve, used directly in the YAML config:
///
/// ```yaml
/// curve:
///   - temp: 30.0
///     duty: 45.0
/// ```
///
/// On deserialization both the current mapping form and the pre-1.1.0
/// `[temp, duty]` sequence form are accepted, so configs written by older
/// releases still parse. It always serializes in the mapping form.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CurvePoint {
    /// The temperature, in °C.
    pub temp: f64,
    /// The fan duty, in percent (0..=100).
    pub duty: f64,
}

impl<'de> Deserialize<'de> for CurvePoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PointVisitor)
    }
}

struct PointVisitor;

impl<'de> Visitor<'de> for PointVisitor {
    type Value = CurvePoint;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "a {{ temp, duty }} mapping or a legacy [temp, duty] sequence"
        )
    }

    fn visit_map<A>(self, mut map: A) -> Result<CurvePoint, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut temp: Option<f64> = None;
        let mut duty: Option<f64> = None;
        while let Some(key) = map.next_key()? {
            match key {
                "temp" => temp = Some(map.next_value()?),
                "duty" => duty = Some(map.next_value()?),
                _ => {
                    let _ignored: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(CurvePoint {
            temp: temp.ok_or_else(|| A::Error::missing_field("temp"))?,
            duty: duty.ok_or_else(|| A::Error::missing_field("duty"))?,
        })
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<CurvePoint, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let temp = seq
            .next_element()?
            .ok_or_else(|| S::Error::custom("expected a [temp, duty] pair"))?;
        let duty = seq
            .next_element()?
            .ok_or_else(|| S::Error::custom("expected a [temp, duty] pair"))?;
        Ok(CurvePoint { temp, duty })
    }
}

impl CurvePoint {
    pub const fn new(temp: f64, duty: f64) -> Self {
        Self { temp, duty }
    }
}

/// The control curve for a single fan.
#[derive(Debug, Clone, Default)]
pub struct Curve {
    /// Points sorted by `temp` ascending.
    pub points: Vec<CurvePoint>,
}

impl Curve {
    pub fn new(points: Vec<CurvePoint>) -> Self {
        let mut points = points;
        points.sort_by(|a, b| {
            a.temp
                .partial_cmp(&b.temp)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { points }
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Evaluate the curve at `temp`, returning a duty percentage clamped to
    /// `0..=100`. Falls back to a sensible default when the curve is empty.
    pub fn evaluate(&self, temp: f64) -> f64 {
        let pts = &self.points;
        if pts.is_empty() {
            return 50.0;
        }

        // Clamp to the endpoints.
        if let Some(first) = pts.first() {
            if temp <= first.temp {
                return first.duty.clamp(0.0, 100.0);
            }
        }
        if let Some(last) = pts.last() {
            if temp >= last.temp {
                return last.duty.clamp(0.0, 100.0);
            }
        }

        // Linear interpolation between the surrounding points.
        for i in 0..pts.len() - 1 {
            let (t0, d0) = (pts[i].temp, pts[i].duty);
            let (t1, d1) = (pts[i + 1].temp, pts[i + 1].duty);
            if temp >= t0 && temp <= t1 {
                if (t1 - t0).abs() < f64::EPSILON {
                    return d1;
                }
                let f = (temp - t0) / (t1 - t0);
                return (d0 + f * (d1 - d0)).clamp(0.0, 100.0);
            }
        }

        pts.last().unwrap().duty.clamp(0.0, 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> Curve {
        Curve::new(vec![
            CurvePoint::new(30.0, 0.0),
            CurvePoint::new(45.0, 40.0),
            CurvePoint::new(60.0, 75.0),
            CurvePoint::new(75.0, 100.0),
        ])
    }

    #[test]
    fn clamps_below_first() {
        assert_eq!(curve().evaluate(10.0), 0.0);
        assert_eq!(curve().evaluate(30.0), 0.0);
    }

    #[test]
    fn clamps_above_last() {
        assert_eq!(curve().evaluate(90.0), 100.0);
        assert_eq!(curve().evaluate(75.0), 100.0);
    }

    #[test]
    fn interpolates_midpoint() {
        // 37.5 is halfway between 30 (0%) and 45 (40%) -> 20%.
        assert!((curve().evaluate(37.5) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn interpolates_across_segment() {
        // 52.5 is the midpoint of 45 (40%) and 60 (75%) -> 57.5%.
        assert!((curve().evaluate(52.5) - 57.5).abs() < 1e-9);
    }

    #[test]
    fn empty_curve_returns_default() {
        assert_eq!(Curve::default().evaluate(50.0), 50.0);
    }

    #[test]
    fn sorts_input_points() {
        let c = Curve::new(vec![
            CurvePoint::new(75.0, 100.0),
            CurvePoint::new(30.0, 0.0),
        ]);
        assert_eq!(c.points[0].temp, 30.0);
        assert_eq!(c.points[1].temp, 75.0);
    }
}
