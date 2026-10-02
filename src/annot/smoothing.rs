//! Stroke smoothing: a lazy-brush stabilizer while drawing, Catmull-Rom curves
//! for the live preview, and RDP simplification + Schneider cubic Bézier
//! fitting when the stroke is finished.

use super::model::Pt;

/// "Lazy brush" stabilizer: the brush only follows the pointer once it is more
/// than `radius` away, which filters out hand jitter without lagging on long moves.
pub struct LazyBrush {
    pub radius: f32,
    pos: Option<Pt>,
}

impl LazyBrush {
    pub fn new(radius: f32) -> Self {
        Self { radius, pos: None }
    }

    /// Feeds a pointer sample; returns the new brush position if it moved.
    pub fn update(&mut self, p: Pt) -> Option<Pt> {
        let Some(cur) = self.pos else {
            self.pos = Some(p);
            return Some(p);
        };
        let d = p.dist(cur);
        if d <= self.radius {
            return None;
        }
        let next = cur.lerp(p, (d - self.radius) / d);
        self.pos = Some(next);
        Some(next)
    }
}

/// Uniform Catmull-Rom spline through `pts`, as a cubic Bézier chain.
pub fn catmull_rom(pts: &[Pt]) -> Vec<Pt> {
    let n = pts.len();
    if n < 2 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    for i in 0..n - 1 {
        let p0 = pts[i.saturating_sub(1)];
        let (p1, p2) = (pts[i], pts[i + 1]);
        let p3 = pts[(i + 2).min(n - 1)];
        out.push(p1.add(p2.sub(p0).scale(1.0 / 6.0)));
        out.push(p2.sub(p3.sub(p1).scale(1.0 / 6.0)));
        out.push(p2);
    }
    out
}

/// Ramer-Douglas-Peucker polyline simplification.
pub fn rdp(pts: &[Pt], eps: f32) -> Vec<Pt> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0, pts.len() - 1)];
    while let Some((s, e)) = stack.pop() {
        let mut best = (0.0f32, 0usize);
        for i in s + 1..e {
            let d = super::geometry::dist_to_segment(pts[i], pts[s], pts[e]);
            if d > best.0 {
                best = (d, i);
            }
        }
        if best.0 > eps {
            keep[best.1] = true;
            stack.push((s, best.1));
            stack.push((best.1, e));
        }
    }
    pts.iter().zip(keep).filter(|(_, k)| *k).map(|(p, _)| *p).collect()
}

/// Turns raw stroke samples into a smooth Bézier chain. `px` is the size of one
/// screen pixel in the samples' units, so smoothing is relative to what the user saw.
pub fn finish_stroke(raw: &[Pt], px: f32) -> Vec<Pt> {
    let mut pts: Vec<Pt> = Vec::with_capacity(raw.len());
    for &p in raw {
        if pts.last().is_none_or(|l: &Pt| l.dist(p) > 0.5 * px) {
            pts.push(p);
        }
    }
    if pts.len() < 2 {
        return pts;
    }
    // Pointer positions are quantized to whole pixels; simplify below that noise.
    let pts = rdp(&pts, 0.6 * px);
    if pts.len() == 2 {
        return catmull_rom(&pts);
    }
    // Split at sharp corners so they stay sharp; fit each piece separately. Short
    // segments are pixel staircase steps, not corners the user drew.
    let mut out = vec![pts[0]];
    let mut start = 0;
    for i in 1..pts.len() {
        let corner = i < pts.len() - 1 && {
            let (a, b) = (pts[i].sub(pts[i - 1]), pts[i + 1].sub(pts[i]));
            a.len() > 4.0 * px && b.len() > 4.0 * px && a.normalized().dot(b.normalized()) < 0.25 // > ~75 degrees
        };
        if corner || i == pts.len() - 1 {
            fit_curve(&pts[start..=i], 0.6 * px, &mut out);
            start = i;
        }
    }
    out
}

/// Schneider's algorithm ("An Algorithm for Automatically Fitting Digitized
/// Curves", Graphics Gems 1990). Appends `c1, c2, p` triples to `out`, whose
/// last element must already be `pts[0]`.
pub fn fit_curve(pts: &[Pt], max_err: f32, out: &mut Vec<Pt>) {
    let n = pts.len();
    if n < 2 {
        return;
    }
    let t1 = pts[1].sub(pts[0]).normalized();
    let t2 = pts[n - 2].sub(pts[n - 1]).normalized();
    fit_cubic(pts, t1, t2, max_err, out, 0);
}

fn fit_cubic(d: &[Pt], t1: Pt, t2: Pt, max_err: f32, out: &mut Vec<Pt>, depth: u32) {
    let n = d.len();
    if n == 2 || depth > 32 {
        let dist = d[0].dist(d[n - 1]) / 3.0;
        out.extend([d[0].add(t1.scale(dist)), d[n - 1].add(t2.scale(dist)), d[n - 1]]);
        return;
    }
    let mut u = chord_params(d);
    let mut bez = generate_bezier(d, &u, t1, t2);
    let (mut err, mut split) = max_error(d, &bez, &u);
    // Schneider only measures error at the sample points; also reject curves that
    // bulge out between them, which shows up as overshoot at tight turns.
    if err < max_err && deviation(&bez, d) < max_err {
        out.extend_from_slice(&bez[1..]);
        return;
    }
    if err < max_err * 4.0 {
        for _ in 0..4 {
            u = reparameterize(d, &u, &bez);
            bez = generate_bezier(d, &u, t1, t2);
            (err, split) = max_error(d, &bez, &u);
            if err < max_err && deviation(&bez, d) < max_err {
                out.extend_from_slice(&bez[1..]);
                return;
            }
        }
    }
    let split = split.clamp(1, n - 2);
    let mut center = d[split - 1].sub(d[split + 1]).normalized();
    if center.len() < 0.5 {
        center = d[split - 1].sub(d[split]).normalized();
    }
    fit_cubic(&d[..=split], t1, center, max_err, out, depth + 1);
    fit_cubic(&d[split..], center.scale(-1.0), t2, max_err, out, depth + 1);
}

/// Largest distance from points along the curve to the input polyline.
fn deviation(b: &[Pt; 4], d: &[Pt]) -> f32 {
    (1..16)
        .map(|i| super::geometry::dist_to_polyline(bezier_at(b, i as f32 / 16.0), d))
        .fold(0.0, f32::max)
}

fn chord_params(d: &[Pt]) -> Vec<f32> {
    let mut u = vec![0.0f32; d.len()];
    for i in 1..d.len() {
        u[i] = u[i - 1] + d[i].dist(d[i - 1]);
    }
    let total = *u.last().unwrap();
    if total > 0.0 {
        u.iter_mut().for_each(|v| *v /= total);
    }
    u
}

fn bezier_at(b: &[Pt; 4], t: f32) -> Pt {
    let mt = 1.0 - t;
    b[0].scale(mt * mt * mt)
        .add(b[1].scale(3.0 * mt * mt * t))
        .add(b[2].scale(3.0 * mt * t * t))
        .add(b[3].scale(t * t * t))
}

fn generate_bezier(d: &[Pt], u: &[f32], t1: Pt, t2: Pt) -> [Pt; 4] {
    let (first, last) = (d[0], d[d.len() - 1]);
    let mut c = [[0.0f32; 2]; 2];
    let mut x = [0.0f32; 2];
    for (p, &t) in d.iter().zip(u) {
        let mt = 1.0 - t;
        let (b0, b1, b2, b3) = (mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t);
        let a1 = t1.scale(b1);
        let a2 = t2.scale(b2);
        c[0][0] += a1.dot(a1);
        c[0][1] += a1.dot(a2);
        c[1][1] += a2.dot(a2);
        let tmp = p.sub(first.scale(b0 + b1)).sub(last.scale(b2 + b3));
        x[0] += a1.dot(tmp);
        x[1] += a2.dot(tmp);
    }
    c[1][0] = c[0][1];
    let det = c[0][0] * c[1][1] - c[1][0] * c[0][1];
    let (mut al, mut ar) = (0.0, 0.0);
    if det.abs() > 1e-12 {
        al = (x[0] * c[1][1] - x[1] * c[0][1]) / det;
        ar = (c[0][0] * x[1] - c[1][0] * x[0]) / det;
    }
    let seg = first.dist(last);
    let eps = 1e-6 * seg;
    if al < eps || ar < eps {
        al = seg / 3.0;
        ar = seg / 3.0;
    }
    [first, first.add(t1.scale(al)), last.add(t2.scale(ar)), last]
}

fn max_error(d: &[Pt], b: &[Pt; 4], u: &[f32]) -> (f32, usize) {
    let mut best = (0.0, d.len() / 2);
    for i in 1..d.len() - 1 {
        let e = bezier_at(b, u[i]).dist(d[i]);
        if e >= best.0 {
            best = (e, i);
        }
    }
    best
}

/// One Newton-Raphson step per sample to improve the parameter values.
fn reparameterize(d: &[Pt], u: &[f32], b: &[Pt; 4]) -> Vec<f32> {
    let d1 = [b[1].sub(b[0]).scale(3.0), b[2].sub(b[1]).scale(3.0), b[3].sub(b[2]).scale(3.0)];
    let d2 = [d1[1].sub(d1[0]).scale(2.0), d1[2].sub(d1[1]).scale(2.0)];
    d.iter()
        .zip(u)
        .map(|(p, &t)| {
            let mt = 1.0 - t;
            let q = bezier_at(b, t);
            let q1 = d1[0].scale(mt * mt).add(d1[1].scale(2.0 * mt * t)).add(d1[2].scale(t * t));
            let q2 = d2[0].scale(mt).add(d2[1].scale(t));
            let diff = q.sub(*p);
            let den = q1.dot(q1) + diff.dot(q2);
            if den.abs() < 1e-12 { t } else { (t - diff.dot(q1) / den).clamp(0.0, 1.0) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annot::geometry::{dist_to_polyline, flatten_bezier};

    #[test]
    fn lazy_brush_ignores_jitter() {
        let mut b = LazyBrush::new(2.0);
        assert_eq!(b.update(Pt::new(0.0, 0.0)), Some(Pt::new(0.0, 0.0)));
        assert_eq!(b.update(Pt::new(1.0, 1.0)), None);
        let p = b.update(Pt::new(10.0, 0.0)).unwrap();
        assert!((p.x - 8.0).abs() < 1e-4 && p.y.abs() < 1e-4);
    }

    #[test]
    fn rdp_drops_collinear_points() {
        let pts: Vec<Pt> = (0..10).map(|i| Pt::new(i as f32, 0.0)).collect();
        assert_eq!(rdp(&pts, 0.1), vec![Pt::new(0.0, 0.0), Pt::new(9.0, 0.0)]);
    }

    #[test]
    fn fitted_curve_stays_within_tolerance() {
        // A noisy-free sine wave sampled like a mouse would.
        let raw: Vec<Pt> =
            (0..400).map(|i| Pt::new(i as f32 * 0.5, (i as f32 * 0.03).sin() * 40.0)).collect();
        let curve = finish_stroke(&raw, 1.0);
        assert_eq!((curve.len() - 1) % 3, 0);
        assert!(curve.len() < raw.len() / 4, "fit should compress: {}", curve.len());
        let flat = flatten_bezier(&curve, 16);
        for p in &raw {
            assert!(dist_to_polyline(*p, &flat) < 1.5, "point {p:?} too far from fit");
        }
    }

    #[test]
    fn corners_are_preserved() {
        let mut raw: Vec<Pt> = (0..=50).map(|i| Pt::new(i as f32, 0.0)).collect();
        raw.extend((1..=50).map(|i| Pt::new(50.0, i as f32)));
        let curve = finish_stroke(&raw, 1.0);
        let flat = flatten_bezier(&curve, 16);
        assert!(dist_to_polyline(Pt::new(50.0, 0.0), &flat) < 0.5);
    }
}

#[cfg(test)]
mod overshoot {
    use super::*;
    use crate::annot::geometry::{dist_to_polyline, flatten_bezier};

    /// Samples a polyline like a mouse would: fast in the middle of each segment
    /// (sparse points), slow near the turns (dense points), rounded to pixels.
    fn mouse_samples(corners: &[Pt]) -> Vec<Pt> {
        let mut out = Vec::new();
        for w in corners.windows(2) {
            let n = 14;
            for i in 0..n {
                // Ease in/out: dense near the ends.
                let t = i as f32 / n as f32;
                let e = t * t * (3.0 - 2.0 * t);
                let p = w[0].lerp(w[1], e);
                out.push(Pt::new(p.x.round(), p.y.round()));
            }
        }
        out.push(*corners.last().unwrap());
        out
    }

    /// How far the curve strays outside the input polyline.
    fn overshoot(curve: &[Pt], input: &[Pt]) -> f32 {
        flatten_bezier(curve, 24).iter().map(|p| dist_to_polyline(*p, input)).fold(0.0, f32::max)
    }

    fn shapes() -> Vec<(&'static str, Vec<Pt>)> {
        let p = |x: f32, y: f32| Pt::new(x, y);
        vec![
            ("check", vec![p(0.0, 20.0), p(8.0, 30.0), p(40.0, 0.0)]),
            ("zigzag", vec![p(0.0, 0.0), p(6.0, 12.0), p(12.0, 0.0), p(18.0, 12.0), p(24.0, 0.0), p(30.0, 12.0)]),
            ("big zigzag", vec![p(0.0, 0.0), p(30.0, 60.0), p(60.0, 0.0), p(90.0, 60.0)]),
            ("hook", vec![p(0.0, 0.0), p(50.0, 0.0), p(46.0, 6.0)]),
            ("square", vec![p(0.0, 0.0), p(40.0, 0.0), p(40.0, 40.0), p(0.0, 40.0), p(0.0, 2.0)]),
        ]
    }

    /// Cursive-like strokes sampled at a varying speed and passed through the stabilizer.
    fn handwriting() -> Vec<(&'static str, Vec<Pt>)> {
        type Curve = Box<dyn Fn(f32) -> Pt>;
        let curves: Vec<(&'static str, Curve)> = vec![
            ("loops", Box::new(|t: f32| Pt::new(t * 120.0 + 14.0 * (t * 25.0).cos(), 18.0 * (t * 25.0).sin()))),
            ("s-curve", Box::new(|t: f32| Pt::new(30.0 * (t * 6.3).sin(), t * 80.0))),
            ("scribble", Box::new(|t: f32| Pt::new(t * 60.0 + 8.0 * (t * 40.0).sin(), 25.0 * (t * 13.0).sin() * (t * 3.0).cos()))),
            ("tight e", Box::new(|t: f32| { let a = t * 9.0; Pt::new(t * 40.0 + 7.0 * a.cos(), 7.0 * a.sin()) })),
        ];
        curves
            .into_iter()
            .map(|(name, f)| {
                let mut brush = LazyBrush::new(2.5);
                let mut t = 0.0f32;
                let mut pts = Vec::new();
                while t <= 1.0 {
                    let p = f(t);
                    if let Some(b) = brush.update(Pt::new(p.x.round(), p.y.round())) {
                        pts.push(b);
                    }
                    // Speed varies 3x along the stroke.
                    t += 0.0015 * (2.0 + (t * 17.0).sin());
                }
                (name, pts)
            })
            .collect()
    }

    #[test]
    fn finished_strokes_do_not_overshoot() {
        let sharp = shapes().into_iter().map(|(n, c)| (n, mouse_samples(&c), c));
        let curvy = handwriting().into_iter().map(|(n, raw)| (n, raw.clone(), raw));
        for (name, raw, reference) in sharp.chain(curvy) {
            let fit = finish_stroke(&raw, 1.0);
            let o = overshoot(&fit, &reference);
            assert!(o < 0.8, "{name}: final curve overshoots by {o:.2}px");
        }
    }
}
