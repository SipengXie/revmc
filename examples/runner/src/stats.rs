//! Student-t distribution tail used by Welch's two-sample t-test.
//! Lanczos `ln_gamma` + continued-fraction regularized incomplete beta,
//! matching the copies in `ablation_bench.rs`, `fullblock_ablation.rs` and
//! `per_contract_ablation.rs`.

fn ln_gamma(x: f64) -> f64 {
    const G: f64 = 7.0;
    const COEFF: [f64; 9] = [
        0.99999999999980993,
        676.5203681218851,
        -1259.1392167224028,
        771.32342877765313,
        -176.61502916214059,
        12.507343278686905,
        -0.13857109526572012,
        9.9843695780195716e-6,
        1.5056327351493116e-7,
    ];
    let x = x - 1.0;
    let mut sum = COEFF[0];
    for (i, &c) in COEFF[1..].iter().enumerate() {
        sum += c / (x + i as f64 + 1.0);
    }
    let t = x + G + 0.5;
    0.5 * (2.0 * std::f64::consts::PI).ln() + (t.ln() * (x + 0.5)) - t + sum.ln()
}

fn regularized_incomplete_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    if x > (a + 1.0) / (a + b + 2.0) {
        return 1.0 - regularized_incomplete_beta(b, a, 1.0 - x);
    }
    let log_prefix =
        a * x.ln() + b * (1.0 - x).ln() - ln_gamma(a) - ln_gamma(b) + ln_gamma(a + b) - a.ln();
    const TINY: f64 = 1e-30;
    const EPS: f64 = 1e-14;
    let mut c = 1.0;
    let mut d = 1.0 - (a + b) * x / (a + 1.0);
    if d.abs() < TINY {
        d = TINY;
    }
    d = 1.0 / d;
    let mut result = d;
    for m in 1..=200 {
        let m_f = m as f64;
        let num_even = m_f * (b - m_f) * x / ((a + 2.0 * m_f - 1.0) * (a + 2.0 * m_f));
        d = 1.0 + num_even * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num_even / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        result *= c * d;
        let num_odd = -(a + m_f) * (a + b + m_f) * x / ((a + 2.0 * m_f) * (a + 2.0 * m_f + 1.0));
        d = 1.0 + num_odd * d;
        if d.abs() < TINY {
            d = TINY;
        }
        c = 1.0 + num_odd / c;
        if c.abs() < TINY {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = c * d;
        result *= delta;
        if (delta - 1.0).abs() < EPS {
            break;
        }
    }
    log_prefix.exp() * result
}

/// CDF of Student's t with `df` degrees of freedom.
pub fn t_cdf(t: f64, df: f64) -> f64 {
    let x = df / (df + t * t);
    let ib = regularized_incomplete_beta(df / 2.0, 0.5, x);
    if t >= 0.0 {
        1.0 - 0.5 * ib
    } else {
        0.5 * ib
    }
}

#[cfg(test)]
mod tests {
    use super::t_cdf;

    #[test]
    fn two_sided_p_matches_published_t_table() {
        // Critical values at alpha = 0.05, two-sided.
        for (df, crit) in [(4.0, 2.776), (8.0, 2.306), (30.0, 2.042), (1000.0, 1.962)] {
            let p = 2.0 * (1.0 - t_cdf(crit, df));
            assert!((p - 0.05).abs() < 5e-4, "df={df}: p={p}");
        }
    }

    #[test]
    fn small_df_tail_is_much_heavier_than_the_normal_approximation() {
        // The bug this replaced: at n=5 per arm (df ~ 8) the normal
        // approximation reports 0.0455 for t=2.0, which crosses alpha=0.05.
        let p_t = 2.0 * (1.0 - t_cdf(2.0, 8.0));
        assert!(p_t > 0.05, "t-based p should not clear alpha at df=8: {p_t}");
    }
}

/// Paired t-test over matched observations. Returns `(t, p)` for the
/// two-sided test on the mean of the per-pair differences `a[i] - b[i]`.
///
/// Use this when the two samples are matched by construction — here, the
/// same contract measured in the same (block, round) under two dispatch
/// modes. An unpaired Welch test on the same data leaves the between-block
/// workload variance in the denominator and, when several rounds replay one
/// block, counts repeated replays as independent observations.
pub fn paired_t(a: &[f64], b: &[f64]) -> (f64, f64) {
    let n = a.len().min(b.len());
    if n < 2 {
        return (0.0, 1.0);
    }
    let d: Vec<f64> = (0..n).map(|i| a[i] - b[i]).collect();
    let mean = d.iter().sum::<f64>() / n as f64;
    let var = d.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    if var <= 0.0 {
        // Every pair differs by the same amount: perfect separation, so the
        // t statistic diverges. Zero difference is the no-effect case.
        return if mean == 0.0 { (0.0, 1.0) } else { (f64::INFINITY.copysign(mean), 0.0) };
    }
    let se = (var / n as f64).sqrt();
    let t = mean / se;
    let p = 2.0 * (1.0 - t_cdf(t.abs(), n as f64 - 1.0));
    (t, p.clamp(0.0, 1.0))
}

#[cfg(test)]
mod paired_tests {
    use super::paired_t;

    #[test]
    fn paired_detects_consistent_shift_unpaired_would_miss() {
        // Large between-pair spread, tiny but consistent within-pair delta.
        // Between-pair spread is huge (100..1050) while each pair differs
        // by ~2 with small noise — the case an unpaired test cannot see.
        let a: Vec<f64> = (0..20).map(|i| 100.0 + i as f64 * 50.0).collect();
        let b: Vec<f64> =
            a.iter().enumerate().map(|(i, x)| x - 2.0 + (i % 3) as f64 * 0.1).collect();
        let (t, p) = paired_t(&a, &b);
        assert!(t > 10.0, "t = {t}");
        assert!(p < 1e-6, "p = {p}");
    }

    #[test]
    fn paired_diverges_on_constant_difference() {
        let a = [1.0, 2.0, 3.0, 4.0];
        let b = [0.0, 1.0, 2.0, 3.0];
        let (t, p) = paired_t(&a, &b);
        assert!(t.is_infinite() && t > 0.0, "t = {t}");
        assert_eq!(p, 0.0);
    }

    #[test]
    fn paired_reports_no_effect_on_identical_samples() {
        let a = [1.0, 2.0, 3.0, 4.0];
        let (t, p) = paired_t(&a, &a);
        assert_eq!(t, 0.0);
        assert_eq!(p, 1.0);
    }
}
