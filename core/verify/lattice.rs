// Verified copy of core/src/solve.rs `lattice` and `gcd`, run with core/verify/run.sh. Keep it
// in step with solve.rs: a change there is a change here. Operators that Verus models as
// truncating (signed / and %) go through one-line helpers with proven specs, rem_euclid gets
// its std meaning through assume_specification.
use vstd::prelude::*;

verus! {

pub assume_specification[i128::rem_euclid](x: i128, m: i128) -> (r: i128)
    requires m > 0,
    returns ((x as int) % (m as int)) as i128;

pub open spec fn abs(x: int) -> int { if x < 0 { -x } else { x } }

/// `v` is a value of the stepped range `lo..=hi` step `s`.
pub open spec fn on(v: int, lo: int, hi: int, s: int) -> bool {
    lo <= v && v <= hi && (v - lo) % s == 0
}

pub open spec fn both(v: int, a: (i128, i128, i128), b: (i128, i128, i128)) -> bool {
    on(v, a.0 as int, a.1 as int, a.2 as int) && on(v, b.0 as int, b.1 as int, b.2 as int)
}

pub enum Lattice {
    Empty,
    One(i64),
    Two(i64, i64),
    Range(i64, i64, i64),
}

pub open spec fn bez(p: int, q: int, a: int, b: int, g: int) -> bool { p * a + q * b == g }

pub open spec fn cong(v: int, c: int, l: int) -> bool { (v - c) % l == 0 }

pub open spec fn i64_range(x: int) -> bool { i64::MIN <= x && x <= i64::MAX }

// ---- arithmetic lemmas --------------------------------------------------------------------

proof fn lemma_quot(n: int, k: int) -> (q: int)
    requires k > 0, n % k == 0,
    ensures n == k * q,
{
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(n, k);
    n / k
}

proof fn lemma_mult_mod(k: int, q: int)
    requires k > 0,
    ensures (k * q) % k == 0,
{
    vstd::arithmetic::div_mod::lemma_mod_multiples_basic(q, k);
    assert(k * q == q * k) by (nonlinear_arith);
}

/// `n % k == 0` from a witness.
proof fn lemma_divides(n: int, k: int, q: int)
    requires k > 0, n == k * q,
    ensures n % k == 0,
{
    lemma_mult_mod(k, q);
}

proof fn lemma_step_abs(p0: int, p1: int, q: int, r0: int, r1: int, b: int)
    requires
        p0 * p1 <= 0, q >= 0, r1 > 0, q * r1 <= r0,
        abs(p1) * r0 + abs(p0) * r1 == b,
    ensures
        abs(p0 - q * p1) == abs(p0) + q * abs(p1),
        abs(p0 - q * p1) * r1 + abs(p1) * (r0 - q * r1) == b,
        p1 * (p0 - q * p1) <= 0,
{
    if p1 >= 0 && p0 <= 0 {
        assert(q * p1 >= 0) by (nonlinear_arith) requires q >= 0, p1 >= 0;
        assert(abs(p0 - q * p1) == abs(p0) + q * abs(p1));
    } else if p1 <= 0 && p0 >= 0 {
        assert(q * p1 <= 0) by (nonlinear_arith) requires q >= 0, p1 <= 0;
        assert(q * abs(p1) == -(q * p1)) by (nonlinear_arith) requires p1 <= 0, abs(p1) == -p1;
        assert(abs(p0 - q * p1) == abs(p0) + q * abs(p1));
    } else {
        assert(p0 * p1 > 0) by (nonlinear_arith)
            requires (p1 > 0 && p0 > 0) || (p1 < 0 && p0 < 0);
    }
    let (x, y) = (abs(p0), abs(p1));
    assert((x + q * y) * r1 + y * (r0 - q * r1) == y * r0 + x * r1) by (nonlinear_arith);
    assert(p1 * (p0 - q * p1) == p0 * p1 - q * (p1 * p1)) by (nonlinear_arith);
    assert(q * (p1 * p1) >= 0) by (nonlinear_arith) requires q >= 0;
}

/// `x` mod `m` as the truncating `%` computes it, with what the proofs need of it.
fn trem(x: i128, m: i128) -> (r: i128)
    requires m > 0,
    ensures
        ((x as int) - (r as int)) % (m as int) == 0,
        -(m as int) < r < m,
        (r == 0) == ((x as int) % (m as int) == 0),
{
    let r = x % m;
    proof {
        let (xi, mi) = (x as int, m as int);
        if xi >= 0 {
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod(xi, mi);
            vstd::arithmetic::div_mod::lemma_mod_bound(xi, mi);
            assert(r as int == xi % mi);
            lemma_divides(xi - r, mi, xi / mi);
        } else {
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod(-xi, mi);
            vstd::arithmetic::div_mod::lemma_mod_bound(-xi, mi);
            assert(r as int == -((-xi) % mi));
            assert(xi - r == mi * (-((-xi) / mi))) by (nonlinear_arith)
                requires -xi == mi * ((-xi) / mi) + (-xi) % mi, r as int == -((-xi) % mi);
            lemma_divides(xi - r, mi, -((-xi) / mi));
        }
        // r == 0 exactly when m divides x
        if r == 0 {
            assert(xi % mi == 0);
        } else {
            let k = lemma_quot(xi - r, mi);
            if xi % mi == 0 {
                let j = lemma_quot(xi, mi);
                assert(r as int == mi * (j - k)) by (nonlinear_arith)
                    requires xi - r == mi * k, xi == mi * j;
                assert(false) by (nonlinear_arith)
                    requires r as int == mi * (j - k), -mi < r, r < mi, r != 0, mi > 0;
            }
        }
    }
    r
}

/// The exact quotient of `x` by a divisor `g` of it.
fn tdiv_exact(x: i128, g: i128) -> (r: i128)
    requires g > 0, (x as int) % (g as int) == 0,
    ensures r * g == x,
{
    let r = x / g;
    proof {
        let (xi, gi) = (x as int, g as int);
        if xi >= 0 {
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod(xi, gi);
        } else {
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod(-xi, gi);
            let k = lemma_quot(xi, gi);
            assert((-xi) % gi == 0) by {
                assert(-xi == gi * (-k)) by (nonlinear_arith) requires xi == gi * k;
                lemma_divides(-xi, gi, -k);
            }
        }
        assert(r * g == x) by (nonlinear_arith)
            requires
                xi >= 0 ==> (xi == gi * (xi / gi) + xi % gi && xi % gi == 0 && r == xi / gi),
                xi < 0 ==> (-xi == gi * ((-xi) / gi) + (-xi) % gi && (-xi) % gi == 0
                    && r == -((-xi) / gi)),
                xi == x, gi == g;
    }
    r
}

/// Floor of a nonnegative `x` by `m`.
fn tdiv_pos(x: i128, m: i128) -> (r: i128)
    requires m > 0, x >= 0,
    ensures r * m <= x < (r + 1) * m, r >= 0,
{
    let r = x / m;
    proof {
        vstd::arithmetic::div_mod::lemma_fundamental_div_mod(x as int, m as int);
        vstd::arithmetic::div_mod::lemma_mod_bound(x as int, m as int);
        assert(r * m <= x < (r + 1) * m) by (nonlinear_arith)
            requires x == m * r + (x as int) % (m as int), 0 <= (x as int) % (m as int) < m;
        assert(r >= 0) by (nonlinear_arith) requires r * m <= x, x < (r + 1) * m, m > 0, x >= 0;
    }
    r
}

// ---- gcd ----------------------------------------------------------------------------------

fn gcd(a: i128, b: i128) -> (res: (i128, i128))
    requires 0 < a <= i64::MAX, 0 < b <= i64::MAX,
    ensures
        res.0 > 0,
        (a as int) % (res.0 as int) == 0,
        (b as int) % (res.0 as int) == 0,
        -(b as int) <= res.1 <= b,
        exists|q: int| #[trigger] bez(res.1 as int, q, a as int, b as int, res.0 as int),
{
    let (mut r0, mut r1, mut p0, mut p1) = (a, b, 1i128, 0i128);
    let ghost (mut q0, mut q1) = (0int, 1int);
    let ghost (mut x0, mut x1, mut y0, mut y1) = (1int, 0int, 0int, 1int);
    while r1 != 0
        invariant
            0 < r0 <= i64::MAX, 0 <= r1 <= i64::MAX,
            0 < a <= i64::MAX, 0 < b <= i64::MAX,
            r0 == p0 * a + q0 * b, r1 == p1 * a + q1 * b,
            a == x0 * r0 + x1 * r1, b == y0 * r0 + y1 * r1,
            p0 * p1 <= 0,
            abs(p1 as int) * r0 + abs(p0 as int) * r1 == b,
            -(b as int) <= p0 <= b,
        decreases r1,
    {
        let q = r0 / r1;
        proof {
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod(r0 as int, r1 as int);
            vstd::arithmetic::div_mod::lemma_mod_bound(r0 as int, r1 as int);
            assert(q * r1 <= r0 && r0 - q * r1 < r1) by (nonlinear_arith)
                requires r0 == r1 * q + (r0 as int) % (r1 as int),
                    0 <= (r0 as int) % (r1 as int) < r1;
            assert(q >= 0 && q <= r0) by (nonlinear_arith)
                requires q * r1 <= r0, r0 - q * r1 < r1, r1 >= 1, r0 > 0;
            assert(abs(p1 as int) <= b) by (nonlinear_arith)
                requires abs(p1 as int) * r0 + abs(p0 as int) * r1 == b, r0 >= 1, r1 >= 0,
                    abs(p0 as int) >= 0, abs(p1 as int) >= 0;
            assert(-0x4000_0000_0000_0000_0000_0000_0000_0000int <= q * p1
                <= 0x4000_0000_0000_0000_0000_0000_0000_0000int) by (nonlinear_arith)
                requires 0 <= q <= 0x8000_0000_0000_0000int,
                    -0x8000_0000_0000_0000int <= p1 <= 0x8000_0000_0000_0000int;
            lemma_step_abs(p0 as int, p1 as int, q as int, r0 as int, r1 as int, b as int);
        }
        let ghost (or0, or1, op0, op1) = (r0 as int, r1 as int, p0 as int, p1 as int);
        let nr1 = r0 - q * r1;
        let np1 = p0 - q * p1;
        r0 = r1;
        r1 = nr1;
        p0 = p1;
        p1 = np1;
        proof {
            let qi = q as int;
            let (nq0, nq1) = (q1, q0 - qi * q1);
            assert(r1 == p1 * a + nq1 * b) by (nonlinear_arith)
                requires or0 == op0 * a + q0 * b, or1 == op1 * a + q1 * b,
                    r1 == or0 - qi * or1, p1 == op0 - qi * op1, nq1 == q0 - qi * q1;
            let (nx0, nx1, ny0, ny1) = (x0 * qi + x1, x0, y0 * qi + y1, y0);
            assert(a == nx0 * r0 + nx1 * r1 && b == ny0 * r0 + ny1 * r1) by (nonlinear_arith)
                requires a == x0 * or0 + x1 * or1, b == y0 * or0 + y1 * or1,
                    r0 == or1, r1 == or0 - qi * or1, nx0 == x0 * qi + x1, nx1 == x0,
                    ny0 == y0 * qi + y1, ny1 == y0;
            q0 = nq0;
            q1 = nq1;
            x0 = nx0;
            x1 = nx1;
            y0 = ny0;
            y1 = ny1;
        }
    }
    proof {
        lemma_divides(a as int, r0 as int, x0);
        lemma_divides(b as int, r0 as int, y0);
        assert(bez(p0 as int, q0, a as int, b as int, r0 as int));
    }
    (r0, p0)
}

// ---- CRT ----------------------------------------------------------------------------------

/// The values on both progressions are those congruent to `x` modulo `l`.
proof fn lemma_crt(a0: int, sa: int, b0: int, sb: int, g: int, p: int, q: int, x: int, v: int)
    requires
        sa > 0, sb > 0, g > 0, sa % g == 0, sb % g == 0, p * sa + q * sb == g,
        (x - a0) % sa == 0, (x - b0) % sb == 0,
    ensures
        ((v - a0) % sa == 0 && (v - b0) % sb == 0) == ((v - x) % ((sa / g) * sb) == 0),
{
    let ka = lemma_quot(sa, g);
    let m = lemma_quot(sb, g);
    assert(sa / g == ka) by (nonlinear_arith) requires sa == g * ka, g > 0;
    let l = ka * sb;
    assert(ka > 0 && m > 0) by (nonlinear_arith) requires sa == g * ka, sb == g * m, g > 0,
        sa > 0, sb > 0;
    assert(l > 0) by (nonlinear_arith) requires ka > 0, sb > 0, l == ka * sb;
    let i = lemma_quot(x - a0, sa);
    let j = lemma_quot(x - b0, sb);
    if (v - a0) % sa == 0 && (v - b0) % sb == 0 {
        let iv = lemma_quot(v - a0, sa);
        let jv = lemma_quot(v - b0, sb);
        // v - x = sa * k with sb | sa * k
        let k = iv - i;
        assert(v - x == sa * k) by (nonlinear_arith) requires v - a0 == sa * iv, x - a0 == sa * i,
            k == iv - i;
        assert(v - x == sb * (jv - j)) by (nonlinear_arith) requires v - b0 == sb * jv,
            x - b0 == sb * j;
        // m | ka * k, and p * ka + q * m == 1, so m | k
        assert(ka * k == m * (jv - j)) by (nonlinear_arith)
            requires sa * k == sb * (jv - j), sa == g * ka, sb == g * m, g > 0;
        assert(p * ka + q * m == 1) by (nonlinear_arith)
            requires p * sa + q * sb == g, sa == g * ka, sb == g * m, g > 0;
        let t = p * (jv - j) + q * k;
        assert(k == m * t) by (nonlinear_arith)
            requires ka * k == m * (jv - j), p * ka + q * m == 1, t == p * (jv - j) + q * k;
        assert(v - x == l * t) by (nonlinear_arith)
            requires v - x == sa * k, k == m * t, sa == g * ka, sb == g * m, l == ka * sb;
        lemma_divides(v - x, l, t);
    }
    if (v - x) % l == 0 {
        let t = lemma_quot(v - x, l);
        assert(v - a0 == sa * (i + m * t)) by (nonlinear_arith)
            requires v - x == l * t, x - a0 == sa * i, l == ka * sb, sa == g * ka, sb == g * m;
        assert(v - b0 == sb * (j + ka * t)) by (nonlinear_arith)
            requires v - x == l * t, x - b0 == sb * j, l == ka * sb;
        lemma_divides(v - a0, sa, i + m * t);
        lemma_divides(v - b0, sb, j + ka * t);
    }
}

/// A value on both progressions makes `g` divide `b0 - a0`.
proof fn lemma_common_divides(a0: int, sa: int, b0: int, sb: int, g: int, v: int)
    requires sa > 0, sb > 0, g > 0, sa % g == 0, sb % g == 0,
        (v - a0) % sa == 0, (v - b0) % sb == 0,
    ensures (b0 - a0) % g == 0,
{
    let ka = lemma_quot(sa, g);
    let kb = lemma_quot(sb, g);
    let i = lemma_quot(v - a0, sa);
    let j = lemma_quot(v - b0, sb);
    assert(b0 - a0 == g * (ka * i - kb * j)) by (nonlinear_arith)
        requires v - a0 == sa * i, v - b0 == sb * j, sa == g * ka, sb == g * kb;
    lemma_divides(b0 - a0, g, ka * i - kb * j);
}

// ---- lattice ------------------------------------------------------------------------------

fn lattice(a: (i128, i128, i128), b: (i128, i128, i128)) -> (res: Lattice)
    requires
        i64_range(a.0 as int), i64_range(a.1 as int), i64_range(b.0 as int),
        i64_range(b.1 as int), 0 < a.2 <= i64::MAX, 0 < b.2 <= i64::MAX,
    ensures
        match res {
            Lattice::Empty => forall|v: int| !#[trigger] both(v, a, b),
            Lattice::One(f) => both(f as int, a, b)
                && forall|v: int| #[trigger] both(v, a, b) ==> v == f,
            Lattice::Two(x, y) => x < y && both(x as int, a, b) && both(y as int, a, b)
                && forall|v: int| #[trigger] both(v, a, b) ==> v == x || v == y,
            Lattice::Range(mn, mx, st) => st > 0 && mn < mx
                && forall|v: int| #[trigger] both(v, a, b) == on(v, mn as int, mx as int, st as int),
        },
{
    let (a0, a1, sa) = a;
    let (b0, b1, sb) = b;
    let lo = if a0 >= b0 { a0 } else { b0 };
    let hi = if a1 <= b1 { a1 } else { b1 };
    let (g, p) = gcd(sa, sb);
    let ghost qb = choose|q: int| #[trigger] bez(p as int, q, sa as int, sb as int, g as int);
    let d = b0 - a0;
    if lo > hi {
        assert forall|v: int| !#[trigger] both(v, a, b) by {}
        return Lattice::Empty;
    }
    if trem(d, g) != 0 {
        assert forall|v: int| !#[trigger] both(v, a, b) by {
            if both(v, a, b) {
                lemma_common_divides(a0 as int, sa as int, b0 as int, sb as int, g as int, v);
            }
        }
        return Lattice::Empty;
    }
    let m = tdiv_exact(sb, g);
    let ka = tdiv_exact(sa, g);
    proof {
        assert(m > 0 && ka > 0 && m <= sb && ka <= sa) by (nonlinear_arith)
            requires m * g == sb, ka * g == sa, g > 0, sb > 0, sa > 0;
        assert(ka * sb <= 0x4000_0000_0000_0000_0000_0000_0000_0000int) by (nonlinear_arith)
            requires 0 < ka <= 0x8000_0000_0000_0000int, 0 < sb <= 0x8000_0000_0000_0000int;
    }
    let l = ka * sb;
    proof {
        assert(l > 0) by (nonlinear_arith) requires ka > 0, sb > 0, l == ka * sb;
    }
    let dg = tdiv_exact(d, g);
    let pm = trem(p, m);
    let dm = trem(dg, m);
    proof {
        assert(-0x4000_0000_0000_0000_0000_0000_0000_0000int <= pm * dm
            <= 0x4000_0000_0000_0000_0000_0000_0000_0000int) by (nonlinear_arith)
            requires -0x8000_0000_0000_0000int <= pm <= 0x8000_0000_0000_0000int,
                -0x8000_0000_0000_0000int <= dm <= 0x8000_0000_0000_0000int;
    }
    let t = (pm * dm).rem_euclid(m);
    proof {
        vstd::arithmetic::div_mod::lemma_mod_bound((pm * dm) as int, m as int);
        assert(sa * t <= 0x4000_0000_0000_0000_0000_0000_0000_0000int && sa * t >= 0)
            by (nonlinear_arith)
            requires 0 < sa <= 0x8000_0000_0000_0000int, 0 <= t < 0x8000_0000_0000_0000int;
    }
    let x = a0 + sa * t;
    proof {
        // x is on both progressions: t = p * dg (mod m), sa * p = g (mod sb)
        let (sai, sbi, gi, mi, kai) = (sa as int, sb as int, g as int, m as int, ka as int);
        let i1 = lemma_quot((p as int) - (pm as int), mi);
        let i2 = lemma_quot((dg as int) - (dm as int), mi);
        vstd::arithmetic::div_mod::lemma_fundamental_div_mod((pm * dm) as int, mi);
        let i3 = ((pm * dm) as int) / mi;
        // t - p * dg = m * u
        let u = -(i3 + (p as int) * i2 + i1 * (dm as int));
        assert((t as int) - (p as int) * (dg as int) == mi * u) by (nonlinear_arith)
            requires (p as int) - (pm as int) == mi * i1, (dg as int) - (dm as int) == mi * i2,
                (pm * dm) as int == mi * i3 + (t as int),
                u == -(i3 + (p as int) * i2 + i1 * (dm as int));
        let (ti, pi, dgi, di) = (t as int, p as int, dg as int, d as int);
        assert(sai * ti == sai * (pi * dgi) + sai * (mi * u)) by (nonlinear_arith)
            requires ti - pi * dgi == mi * u;
        assert(sai * (pi * dgi) == di - qb * sbi * dgi) by (nonlinear_arith)
            requires pi * sai + qb * sbi == gi, dgi * gi == di;
        assert(sai * (mi * u) == kai * sbi * u) by (nonlinear_arith)
            requires kai * gi == sai, mi * gi == sbi;
        assert(kai * sbi * u - qb * sbi * dgi == sbi * (kai * u - qb * dgi)) by (nonlinear_arith);
        assert((x as int) - (b0 as int) == sbi * (kai * u - qb * (dg as int)));
        lemma_divides((x as int) - (b0 as int), sbi, kai * u - qb * (dg as int));
        lemma_divides((x as int) - (a0 as int), sai, t as int);
        assert(sai / gi == kai) by (nonlinear_arith) requires kai * gi == sai, gi > 0;
        assert forall|v: int| #[trigger] both(v, a, b)
            == (lo <= v <= hi && cong(v, x as int, l as int)) by {
            lemma_crt(a0 as int, sai, b0 as int, sbi, gi, p as int, qb, x as int, v);
        }
    }
    let first = lo + (x - lo).rem_euclid(l);
    proof {
        vstd::arithmetic::div_mod::lemma_mod_bound((x - lo) as int, l as int);
        vstd::arithmetic::div_mod::lemma_fundamental_div_mod((x - lo) as int, l as int);
        let k = ((x - lo) as int) / (l as int);
        assert((first as int) - (x as int) == (l as int) * (-k)) by (nonlinear_arith)
            requires (x - lo) as int == (l as int) * k + ((x - lo) as int) % (l as int),
                first == lo + ((x - lo) as int) % (l as int);
        lemma_divides((first as int) - (x as int), l as int, -k);
        // a value congruent to x is congruent to first, and the smallest at or past lo
        assert forall|v: int| #[trigger] cong(v, x as int, l as int)
            == cong(v, first as int, l as int) by {
            if (v - x) % (l as int) == 0 {
                let j = lemma_quot(v - x, l as int);
                assert(v - first == (l as int) * (j + k)) by (nonlinear_arith)
                    requires v - x == (l as int) * j, (first as int) - (x as int) == (l as int) * (-k);
                lemma_divides(v - first, l as int, j + k);
            }
            if (v - first) % (l as int) == 0 {
                let j = lemma_quot(v - first, l as int);
                assert(v - x == (l as int) * (j - k)) by (nonlinear_arith)
                    requires v - first == (l as int) * j, (first as int) - (x as int) == (l as int) * (-k);
                lemma_divides(v - x, l as int, j - k);
            }
        }
        assert forall|v: int| lo <= v && #[trigger] cong(v, first as int, l as int)
            implies v >= first by {
            let j = lemma_quot(v - first, l as int);
            assert(v >= first) by (nonlinear_arith)
                requires v - first == (l as int) * j, lo <= v, first - lo < l, l > 0,
                    first >= lo;
        }
    }
    if first > hi {
        assert forall|v: int| !#[trigger] both(v, a, b) by {
            if both(v, a, b) {
                assert(cong(v, first as int, l as int));
            }
        }
        return Lattice::Empty;
    }
    let n = tdiv_pos(hi - first, l);
    proof {
        assert(n * l <= hi - first) by (nonlinear_arith) requires n * l <= hi - first;
    }
    let last = first + n * l;
    proof {
        assert forall|v: int| #[trigger] both(v, a, b) == on(v, first as int, last as int, l as int)
            by {
            if both(v, a, b) {
                let j = lemma_quot(v - first, l as int);
                assert(v <= last) by (nonlinear_arith)
                    requires v - first == (l as int) * j, v <= hi, hi - first < (n + 1) * l,
                        last == first + n * l, l > 0, v >= first;
            }
            if on(v, first as int, last as int, l as int) {
                assert(v <= hi);
            }
        }
    }
    let fits = |v: i128| -> (r: Option<i64>)
        ensures r.is_some() == i64_range(v as int), r.is_some() ==> r.unwrap() == v,
    {
        if i64::MIN as i128 <= v && v <= i64::MAX as i128 { Some(v as i64) } else { None }
    };
    let (fo, lo_, so) = (fits(first), fits(last), fits(l));
    if fo.is_some() && first == last {
        proof {
            assert forall|v: int| #[trigger] both(v, a, b) implies v == first by {
                let j = lemma_quot(v - first, l as int);
                assert(v == first) by (nonlinear_arith)
                    requires v - first == (l as int) * j, first <= v, v <= last, last == first,
                        l > 0;
            }
            lemma_divides(0, l as int, 0);
            assert(both(first as int, a, b));
        }
        return Lattice::One(fo.unwrap());
    }
    if fo.is_some() && lo_.is_some() && so.is_some() {
        proof {
            assert(first < last);
        }
        return Lattice::Range(fo.unwrap(), lo_.unwrap(), so.unwrap());
    }
    if fo.is_some() && lo_.is_some() {
        proof {
            // l past i64 fits once in a span of at most 2^64 - 1, so last == first + l
            assert(n == 1) by (nonlinear_arith)
                requires n * l <= hi - first, first < last, last == first + n * l,
                    l > 0x7fff_ffff_ffff_ffffint, hi - first <= 0xffff_ffff_ffff_ffffint, n >= 0;
            assert forall|v: int| #[trigger] both(v, a, b) implies v == first || v == last by {
                let j = lemma_quot(v - first, l as int);
                assert(j == 0 || j == 1) by (nonlinear_arith)
                    requires v - first == (l as int) * j, first <= v, v <= last,
                        last == first + l, l > 0;
                assert(v == first || v == last) by (nonlinear_arith)
                    requires v - first == (l as int) * j, j == 0 || j == 1, last == first + l;
            }
            lemma_divides(0, l as int, 0);
            lemma_divides((last as int) - (first as int), l as int, 1);
            assert(both(first as int, a, b) && both(last as int, a, b));
        }
        return Lattice::Two(fo.unwrap(), lo_.unwrap());
    }
    proof {
        // first and last lie in lo..=hi, inside i64
        assert(false);
    }
    Lattice::Empty
}

fn main() {}

} // verus!
