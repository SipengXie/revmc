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
